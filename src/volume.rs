//! How many rows a table can actually hold.
//!
//! The row count was a flat number per table, which is fine until a unique
//! constraint runs out of values to be unique over. A `bool UNIQUE` holds two
//! rows. An enum of three labels holds three. A join table holds as many rows
//! as there are pairs to join, and if one of its parents is itself capped at
//! two, that is not many.
//!
//! Asking for fifty of those is not a hard problem, it is an impossible one,
//! and there are only two honest answers: write fewer rows, or refuse. This
//! module works out which, by computing a bound it can point at.
//!
//! Everything here is an *upper* bound and every unknown is treated as no
//! bound at all. That direction is deliberate. Overstating a bound writes rows
//! the database throws out; understating one writes fewer rows than it might
//! have. The first is the failure this project exists to prevent, the second
//! is a smaller table.

use std::collections::BTreeMap;

use crate::schema::{ColumnType, ForeignKey, Table, TableId, UniqueKey};

/// How many distinct values a type holds, when that number is small enough to
/// matter.
///
/// `None` is "no bound worth counting" rather than "unbounded" — an `int` has
/// four billion values, and no row count will ever reach them.
pub fn domain_size(type_: &ColumnType) -> Option<usize> {
    match type_ {
        ColumnType::Boolean => Some(2),
        ColumnType::Enum { labels, .. } => Some(labels.len()),
        // A domain is its inner type plus constraints. Constraints only ever
        // narrow it, so the inner type's size stays an upper bound.
        ColumnType::Domain { inner, .. } => domain_size(inner),
        _ => None,
    }
}

/// The largest number of distinct rows this table can hold.
///
/// `written` is how many rows each already-generated table actually received,
/// not how many were asked for — a parent that was itself capped caps its
/// children in turn, and that has to propagate down the graph rather than
/// being recomputed from the requested figure.
///
/// `None` means no bound was found. That is not the same as there being none.
pub fn capacity(table: &Table, written: &BTreeMap<TableId, usize>) -> Option<usize> {
    table
        .unique_keys
        .iter()
        .filter_map(|key| key_capacity(table, key, written))
        .min()
}

/// How many rows every table is going to receive.
///
/// Worked out in insert order, because a cap on a parent is a cap on its
/// children and the order is the thing that makes that propagate. Computed
/// once and shared, so the figure the report quotes is the figure the SQL
/// uses rather than a second guess at it.
pub fn plan(
    schema: &crate::schema::Schema,
    order: &[TableId],
    requested: usize,
) -> BTreeMap<TableId, usize> {
    let mut counts: BTreeMap<TableId, usize> = BTreeMap::new();
    for id in order {
        let Some(table) = schema.get(id) else { continue };
        let n = capacity(table, &counts).map_or(requested, |c| c.min(requested));
        counts.insert(id.clone(), n);
    }
    counts
}

/// The bound one unique key imposes: the product of what each of its columns
/// can independently hold.
fn key_capacity(
    table: &Table,
    key: &UniqueKey,
    written: &BTreeMap<TableId, usize>,
) -> Option<usize> {
    let mut product: usize = 1;
    let mut counted: Vec<&str> = Vec::new();

    for name in &key.columns {
        let bound = if let Some(fk) = covering_key(table, name) {
            // A composite foreign key is drawn as one row from the parent, so
            // its columns are one choice between them and must not be
            // multiplied out as though they varied independently.
            if counted.contains(&fk.name.as_str()) {
                continue;
            }
            counted.push(&fk.name);
            *written.get(&fk.references)?
        } else {
            domain_size(&table.column(name)?.type_)?
        };
        product = product.checked_mul(bound)?;
    }

    Some(product)
}

/// The foreign key covering a column, if one does.
fn covering_key<'t>(table: &'t Table, column: &str) -> Option<&'t ForeignKey> {
    table
        .foreign_keys
        .iter()
        .find(|fk| fk.columns.iter().any(|c| c == column))
}

/// Columns the generator must vary per row, so that no unique key repeats.
///
/// A single-column key is the easy case and was the only one handled: vary
/// that column and it is unique. A composite key had nothing done for it at
/// all, so `PRIMARY KEY (first_name, last_name)` drew both names from a
/// sixteen-word list and collided almost immediately.
///
/// A tuple is distinct as soon as *one* of its columns is, so only one column
/// per key needs to vary. It has to be one that actually can: not drawn from a
/// parent, where the values are whatever the parent had, and not of a type
/// with fewer values than there are rows — varying a boolean gives two of
/// everything, whatever the row index says.
///
/// **Known limit:** a composite key made entirely of bounded columns —
/// `UNIQUE (is_active, status)` — has no column that can carry the whole
/// tuple's distinctness, and would need the columns walked as an odometer.
/// `capacity` still caps the row count at the product, so the failure is a
/// duplicate rather than an overflow, but it is a gap and it is not closed.
pub fn varying_columns(table: &Table) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for key in &table.unique_keys {
        if key.columns.len() == 1 {
            out.insert(key.columns[0].clone());
            continue;
        }
        let chosen = key.columns.iter().find(|name| {
            covering_key(table, name).is_none()
                && table
                    .column(name)
                    .is_some_and(|c| domain_size(&c.type_).is_none())
        });
        if let Some(name) = chosen {
            out.insert(name.clone());
        }
    }
    out
}

/// How far apart consecutive rows step through each parent's pool.
///
/// Where a unique key is made entirely of foreign keys — a join table — the
/// combinations must not repeat. Cycling each key independently repeats as
/// soon as two of them share a period: two parents of two rows each give
/// `(a0,b0) (a1,b1) (a0,b0)` on the third row. Walking them as digits of one
/// number instead — the first advancing every row, the second only when the
/// first wraps — enumerates every pair before reusing any.
///
/// Keys not named here are absent from the result and keep cycling on their
/// own, which is what makes an ordinary child table's parents vary per row
/// instead of every row pointing at the same one.
///
/// **Known limit:** two overlapping all-foreign-key unique keys on one table —
/// `PRIMARY KEY (a, b)` alongside `UNIQUE (b, c)` — cannot both be enumerated
/// by one odometer. The narrower is chosen and the other is left to the
/// database, which will reject rather than accept a duplicate. Loud, not
/// silent, but it is a gap.
pub fn strides(
    table: &Table,
    written: &BTreeMap<TableId, usize>,
) -> BTreeMap<String, usize> {
    let Some(key) = joining_key(table, written) else {
        return BTreeMap::new();
    };

    let mut strides = BTreeMap::new();
    let mut step = 1usize;
    for fk in &table.foreign_keys {
        if !key.columns.iter().any(|c| fk.columns.contains(c)) {
            continue;
        }
        let Some(size) = written.get(&fk.references).copied() else {
            continue;
        };
        if size == 0 {
            continue;
        }
        strides.insert(fk.name.clone(), step);
        step = step.saturating_mul(size);
    }
    strides
}

/// The narrowest unique key whose every column comes from a foreign key.
fn joining_key<'t>(
    table: &'t Table,
    written: &BTreeMap<TableId, usize>,
) -> Option<&'t UniqueKey> {
    table
        .unique_keys
        .iter()
        .filter(|key| {
            key.columns.len() > 1
                && key.columns.iter().all(|c| {
                    covering_key(table, c)
                        .is_some_and(|fk| written.contains_key(&fk.references))
                })
        })
        .min_by_key(|key| (key_capacity(table, key, written).unwrap_or(usize::MAX), &key.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ForeignKey, UniqueKey};

    fn col(name: &str, type_: ColumnType) -> Column {
        Column {
            name: name.into(),
            type_,
            nullable: false,
            has_default: false,
            default_is_sequence: false,
            is_generated: false,
            position: 1,
        }
    }

    fn table(name: &str, columns: Vec<Column>) -> Table {
        Table {
            id: TableId { schema: "public".into(), name: name.into() },
            columns,
            foreign_keys: vec![],
            unique_keys: vec![],
            checks: vec![],
        }
    }

    fn unique(name: &str, columns: &[&str]) -> UniqueKey {
        UniqueKey {
            name: name.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            is_primary: false,
        }
    }

    fn parent(name: &str) -> TableId {
        TableId { schema: "public".into(), name: name.into() }
    }

    #[test]
    fn a_boolean_holds_two_values_and_an_enum_holds_its_labels() {
        assert_eq!(domain_size(&ColumnType::Boolean), Some(2));
        assert_eq!(
            domain_size(&ColumnType::Enum {
                name: "mood".into(),
                qualified: None,
                labels: vec!["sad".into(), "ok".into(), "happy".into()],
            }),
            Some(3)
        );
        assert_eq!(domain_size(&ColumnType::Integer { bytes: 4 }), None);
        assert_eq!(domain_size(&ColumnType::Text { max_length: None }), None);
    }

    #[test]
    fn a_unique_boolean_caps_the_table_at_two_rows() {
        let mut t = table("flags", vec![col("on_off", ColumnType::Boolean)]);
        t.unique_keys.push(unique("flags_on_off_key", &["on_off"]));
        assert_eq!(capacity(&t, &BTreeMap::new()), Some(2));
    }

    #[test]
    fn a_table_with_nothing_bounded_has_no_bound_found() {
        let mut t = table("users", vec![col("id", ColumnType::Integer { bytes: 4 })]);
        t.unique_keys.push(unique("users_pkey", &["id"]));
        assert_eq!(capacity(&t, &BTreeMap::new()), None);
    }

    #[test]
    fn the_tightest_of_several_keys_is_the_one_that_binds() {
        let mut t = table(
            "thing",
            vec![
                col("id", ColumnType::Integer { bytes: 4 }),
                col("flag", ColumnType::Boolean),
                col(
                    "state",
                    ColumnType::Enum {
                        name: "s".into(),
                        qualified: None,
                        labels: vec!["a".into(), "b".into(), "c".into()],
                    },
                ),
            ],
        );
        t.unique_keys.push(unique("by_id", &["id"]));
        t.unique_keys.push(unique("by_state", &["state"]));
        t.unique_keys.push(unique("by_flag", &["flag"]));
        assert_eq!(capacity(&t, &BTreeMap::new()), Some(2));
    }

    #[test]
    fn a_join_table_holds_as_many_rows_as_there_are_pairs() {
        let mut t = table(
            "grants",
            vec![
                col("user_id", ColumnType::Integer { bytes: 4 }),
                col("role_id", ColumnType::Integer { bytes: 4 }),
            ],
        );
        t.foreign_keys.push(ForeignKey {
            name: "fk_user".into(),
            columns: vec!["user_id".into()],
            references: parent("users"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        t.foreign_keys.push(ForeignKey {
            name: "fk_role".into(),
            columns: vec!["role_id".into()],
            references: parent("roles"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        t.unique_keys.push(unique("grants_pkey", &["user_id", "role_id"]));

        let written = BTreeMap::from([(parent("users"), 10), (parent("roles"), 2)]);
        assert_eq!(capacity(&t, &written), Some(20));

        // And the two are walked as digits, so all twenty pairs come out
        // before any repeats.
        let strides = strides(&t, &written);
        assert_eq!(strides.get("fk_user"), Some(&1));
        assert_eq!(strides.get("fk_role"), Some(&10));
    }

    #[test]
    fn a_composite_foreign_key_is_one_choice_and_not_two() {
        // (a, b) both come from one parent row, so the pair can take as many
        // values as the parent has rows — not as many as the two multiplied.
        let mut t = table(
            "child",
            vec![
                col("a", ColumnType::Integer { bytes: 4 }),
                col("b", ColumnType::Integer { bytes: 4 }),
            ],
        );
        t.foreign_keys.push(ForeignKey {
            name: "fk".into(),
            columns: vec!["a".into(), "b".into()],
            references: parent("p"),
            referenced_columns: vec!["x".into(), "y".into()],
            deferrable: false,
        });
        t.unique_keys.push(unique("child_pkey", &["a", "b"]));
        let written = BTreeMap::from([(parent("p"), 7)]);
        assert_eq!(capacity(&t, &written), Some(7), "7 rows, not 49");
    }

    #[test]
    fn an_unwritten_parent_gives_no_bound_rather_than_a_wrong_one() {
        // A self-reference, or a parent that was refused. Guessing zero here
        // would silently produce an empty table.
        let mut t = table("child", vec![col("parent_id", ColumnType::Integer { bytes: 4 })]);
        t.foreign_keys.push(ForeignKey {
            name: "fk".into(),
            columns: vec!["parent_id".into()],
            references: parent("missing"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        t.unique_keys.push(unique("child_pkey", &["parent_id"]));
        assert_eq!(capacity(&t, &BTreeMap::new()), None);
    }

    #[test]
    fn an_ordinary_child_table_gets_no_strides_and_keeps_cycling() {
        // One foreign key and a surrogate primary key: the parents should vary
        // per row as they always did, not all point at the first one.
        let mut t = table(
            "orders",
            vec![
                col("id", ColumnType::Integer { bytes: 4 }),
                col("user_id", ColumnType::Integer { bytes: 4 }),
            ],
        );
        t.foreign_keys.push(ForeignKey {
            name: "fk".into(),
            columns: vec!["user_id".into()],
            references: parent("users"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        t.unique_keys.push(unique("orders_pkey", &["id"]));
        let written = BTreeMap::from([(parent("users"), 10)]);
        assert!(strides(&t, &written).is_empty());
        assert_eq!(capacity(&t, &written), None);
    }
}
