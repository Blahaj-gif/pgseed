//! Which tables can be filled, and the exact sentence explaining each that
//! cannot.
//!
//! This is the whole doctrine in one module. The failure mode of a seed tool
//! is not that it crashes — it is that it inserts plausible rows which quietly
//! violate a rule nobody re-checked, and then everything downstream is tested
//! against data the real system would have rejected.
//!
//! So the rule is: **never emit a row that cannot be shown to satisfy every
//! constraint that was read.** A table carrying a CHECK this cannot prove it
//! satisfies is not attempted, not approximated and not filled with a best
//! guess. It is named, the constraint is quoted, and it is left alone.
//!
//! Three outcomes, never two — filled, refused, or could-not-read — for the
//! same reason a check that could not run must never count as a check that
//! passed.

use crate::graph::Order;
use crate::schema::{ForeignKey, Schema, Table, TableId};

/// Why a table will not be filled. Each carries enough to print a line a
/// person can act on, which usually means naming the constraint they would
/// have to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A CHECK constraint whose satisfaction cannot be guaranteed.
    CheckConstraint { name: String, definition: String },
    /// A column of a type no value can be produced for.
    UnsupportedType { column: String, type_name: String },
    /// The table sits in a foreign key cycle that cannot be broken.
    UnbreakableCycle { reason: String },
    /// A required column points at a table that is itself refused, so any row
    /// here would reference nothing.
    DependsOnRefused { table: TableId, constraint: String },
    /// Two unique keys that share a column and cannot both be enumerated.
    UnsatisfiableKeys { first: String, second: String },
    /// Two foreign keys that share a column and would disagree about its value.
    EntangledForeignKeys {
        first: String,
        second: String,
        column: String,
    },
    /// A required column points at a table that was never read — in a schema
    /// that was not asked for, or of a kind introspection skips. Whether it
    /// holds any rows to point at is not knowable from here.
    DependsOnUnread { table: TableId, constraint: String },
}

impl Refusal {
    /// One line, for the report. Written to be read by somebody who did not
    /// write this tool and wants to know what to change.
    pub fn explain(&self) -> String {
        match self {
            Refusal::CheckConstraint { name, definition } => format!(
                "CHECK \"{name}\" {definition} — this cannot prove a generated \
                 row satisfies it, so it will not write one"
            ),
            Refusal::UnsupportedType { column, type_name } => {
                format!("column \"{column}\" is of type {type_name}, which has no generator")
            }
            Refusal::UnbreakableCycle { reason } => reason.clone(),
            Refusal::DependsOnRefused { table, constraint } => format!(
                "foreign key \"{constraint}\" requires {table}, which is itself refused"
            ),
            Refusal::UnsatisfiableKeys { first, second } => format!(
                "unique keys \"{first}\" and \"{second}\" share a column with no room to spare, so this cannot make both distinct at once"
            ),
            Refusal::EntangledForeignKeys {
                first,
                second,
                column,
            } => format!(
                "foreign keys \"{first}\" and \"{second}\" both write \"{column}\", so one of them would end up half from one parent row and half from another — a pair neither parent ever held, which this cannot show is satisfied"
            ),
            Refusal::DependsOnUnread { table, constraint } => format!(
                "foreign key \"{constraint}\" requires {table}, which was not read — \
                 pass --schema for it, or this cannot show a row it names exists"
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Verdict {
    /// Tables that can be filled, in insert order.
    pub fillable: Vec<TableId>,
    /// Tables that will not be, each with every reason found.
    pub refused: Vec<(TableId, Vec<Refusal>)>,
    /// Whether any cycle is being populated by deferring its constraints to
    /// commit, which the emitter has to say out loud in SQL.
    pub deferred_constraints: bool,
    /// Cycles broken by leaving a key NULL, as (table, constraint). The
    /// emitter fills these in afterwards: a `manager_id` that is null on every
    /// row is valid and has modelled nothing.
    pub deferred_repairs: Vec<(TableId, String)>,
}

impl Verdict {
    pub fn total(&self) -> usize {
        self.fillable.len() + self.refused.len()
    }

    /// The share of tables this could fill. The number that decides whether
    /// the project is worth continuing, measured before a value is generated.
    pub fn reach(&self) -> f64 {
        if self.total() == 0 {
            return 0.0;
        }
        self.fillable.len() as f64 / self.total() as f64
    }

    pub fn is_refused(&self, id: &TableId) -> bool {
        self.refused.iter().any(|(t, _)| t == id)
    }
}

/// The tightest ceiling on a text column's length, from its declaration and
/// from every CHECK on the table, or `None` where nothing bounds it.
fn ceiling_for(table: &Table, column: &crate::schema::Column) -> Option<i32> {
    use crate::checks::Meaning;
    let declared = match &column.type_ {
        crate::schema::ColumnType::Text { max_length } => *max_length,
        _ => None,
    };
    table
        .checks
        .iter()
        .filter_map(|check| match crate::checks::interpret(&check.definition) {
            Meaning::LengthLimit { column: c, max } | Meaning::ByteLimit { column: c, max }
                if c == column.name =>
            {
                Some(max)
            }
            _ => None,
        })
        .chain(declared)
        .min()
}

/// Reasons a table cannot be filled *on its own terms*, before anything about
/// what it depends on is considered.
fn direct_refusals(table: &Table, schema: &Schema, order: &Order) -> Vec<Refusal> {
    let mut out = Vec::new();

    // A CHECK is read and matched against a closed set of exact shapes. Two
    // of them — a length limit, and a NOT NULL written as a CHECK — this
    // already satisfies by construction, and measured against real schemas
    // they are 81% of all the CHECK constraints there are. Everything else is
    // refused, unexamined and unapproximated. See `checks`.
    for check in &table.checks {
        use crate::checks::Meaning;
        let satisfiable = crate::checks::interpret_all(&check.definition)
            .into_iter()
            .all(|meaning| match meaning {
                // Already true by construction, or a limit the generator honours.
                Meaning::LengthLimit { column, .. }
                | Meaning::ByteLength { column, .. }
                | Meaning::LowerBound { column, .. }
                | Meaning::Lowercase { column } => table.column(&column).is_some(),
                // A column obliged to hold a value cannot also be obliged to
                // be null, and a table can carry one rule of each over the
                // same column. Discourse's `topics` does:
                //
                //   (category_id IS NOT NULL) OR (archetype <> 'regular')
                //   (category_id IS NULL)     OR (archetype <> 'private_message')
                //
                // Each disjunction is satisfiable on its own, by filling the
                // column or by nulling it. Both at once is not, and the way
                // out — writing an `archetype` that is neither value — means
                // solving the disjunction rather than recognising it. So this
                // is refused, and the alternative was writing NULL and having
                // the database reject the row, which is what it did.
                Meaning::NotNull { column } => {
                    table.column(&column).is_some() && !table.check_forces_null(&column)
                }
                // An obligation rather than a permission: satisfied by writing
                // NULL, and therefore only satisfiable if the column may BE null.
                // A NOT NULL column carrying `(col IS NULL) OR ...` genuinely
                // cannot take this way out, and accepting it would produce rows
                // that violate the constraint the moment they are written.
                Meaning::MustBeNull { column } => table.column(&column).is_some_and(|c| c.nullable),
                // A byte ceiling is a length limit the generator honours; an array
                // limit of one or more is already met, because every array this
                // writes holds one element.
                Meaning::ByteLimit { column, .. } => table.column(&column).is_some(),
                // Every value this generates is at least one character long, so a
                // column that must not be empty already is not.
                Meaning::NonEmpty { column } => table.column(&column).is_some(),
                // A floor on the length is met by padding, but only where there
                // is room to pad into. A column declared `varchar(8)` and obliged
                // to hold twelve characters has no satisfying row, and reading
                // both rules and then writing eight characters anyway is exactly
                // the silent-pass this project exists to not do.
                Meaning::MinLength { column, min } => table
                    .column(&column)
                    .is_some_and(|c| ceiling_for(table, c).map_or(true, |max| max >= min)),
                // One of a listed set of values, which is an enum written out
                // longhand and satisfied the same way.
                Meaning::ValueSet { column, values } => {
                    !values.is_empty() && table.column(&column).is_some()
                }
                // At least one of them holds a value, and all of them are filled
                // unless a foreign key had nowhere to point. One that certainly
                // gets a value is enough, and `filled_column` finds it.
                Meaning::AtLeastOneNonNull { columns } => {
                    // At least one of them has to end up holding a value, so at
                    // least one must not be under an obligation to be null. Every
                    // one of GitLab's `ai_tool_rules` permission columns is, from
                    // a separate `(col IS NULL) OR ...` on each, and the two
                    // rules together have no satisfying row.
                    // `filled_column` is not the right question here: it
                    // answers "which single one holds the value", which is
                    // exactly-one's question, and it gives up when two of them
                    // are NOT NULL — a case that makes at-least-one *certain*
                    // rather than impossible. Synapse and GitLab both write
                    // `num_nonnulls(a, b) = 2` over two NOT NULL columns and
                    // were refused for it.
                    //
                    // A foreign key column is less sure: its parent may have
                    // been refused, leaving it NULL without refusing this
                    // table, since a nullable key spreads no contagion. So a
                    // plain column is looked for first, and only when every
                    // candidate is a foreign key does this fall back to the
                    // stricter question.
                    let certain = |c: &String| {
                        table.column(c).is_some()
                            && !table.check_forces_null(c)
                            && !table.foreign_keys.iter().any(|fk| fk.columns.contains(c))
                    };
                    columns.iter().any(certain)
                        || (columns.iter().any(|c| !table.check_forces_null(c))
                            && crate::generate::filled_column(table, &columns).is_some())
                }
                Meaning::CardinalityLimit { column, max } => {
                    max >= 1 && table.column(&column).is_some()
                }
                // Only for a column that actually holds JSON. `jsonb_typeof` of
                // anything else is a question this cannot answer by generating.
                Meaning::JsonType { column, .. } => table
                    .column(&column)
                    .is_some_and(|c| matches!(c.type_, crate::schema::ColumnType::Json { .. })),
                // Exactly one of these columns holds a value. Satisfiable when a
                // column can be chosen to hold it and every other one may be null
                // — two columns the catalogue insists on cannot have exactly one
                // between them, and `filled_column` returns nothing to say so.
                Meaning::ExactlyOneNonNull { columns } => {
                    let chosen = crate::generate::filled_column(table, &columns);
                    chosen.is_some_and(|keep| {
                        // The one that holds the value must not be under an
                        // obligation elsewhere to be null, and every other one
                        // must be allowed to be.
                        !table.check_forces_null(&keep)
                            && columns.iter().all(|c| {
                                table
                                    .column(c)
                                    .is_some_and(|col| *c == keep || col.nullable)
                            })
                    })
                }
                // A ceiling on a number is honoured by generating under it.
                Meaning::UpperBound { column, .. } => table.column(&column).is_some(),
                Meaning::Unknown => false,
            });
        if !satisfiable {
            out.push(Refusal::CheckConstraint {
                name: check.name.clone(),
                definition: check.definition.clone(),
            });
        }
    }

    // A column with no generator, but only if a row genuinely has to carry a
    // value for it. A defaulted column is left to the database, and a nullable
    // one is simply omitted from the insert — so a `geometry` column refuses
    // its table only when it is NOT NULL with no default. Getting this wrong
    // refuses a great many ordinary tables for a column nobody needed filled.
    for column in table.columns_to_write() {
        if !column.nullable && !column.type_.is_generatable() {
            out.push(Refusal::UnsupportedType {
                column: column.name.clone(),
                type_name: column.type_.describe(),
            });
        }
    }

    // Two composite unique keys sharing a bounded column. One odometer cannot
    // advance a column at two different rates, so satisfying the first and
    // hoping the second falls out is exactly the guess this does not make.
    if let Some((first, second)) = crate::volume::overlapping_keys(table) {
        out.push(Refusal::UnsatisfiableKeys { first, second });
    }

    // Two foreign keys sharing a column. A composite key is drawn one whole
    // parent row at a time, so all of its columns come from that row and the
    // pair provably exists. Two keys sharing a column break exactly that: the
    // second writes the shared column over the first, and the first is left
    // half from one parent row and half from another — a pair neither parent
    // ever held. Postgres said so on Langfuse's `in_app_agent_runs`, which is
    // how this was found, and a row the database refuses is a row this should
    // have refused first.
    if let Some((first, second, column)) = entangled_foreign_keys(table, schema) {
        out.push(Refusal::EntangledForeignKeys {
            first,
            second,
            column,
        });
    }

    if let Some(reason) = order.reason_for(&table.id) {
        out.push(Refusal::UnbreakableCycle {
            reason: reason.to_string(),
        });
    }

    out
}

/// Two foreign keys that share a column, and the column they disagree over.
///
/// The same constraint written twice is not a disagreement: identical columns
/// pointing at identical columns of the same table cannot pull in two
/// directions, so those are passed over rather than refused.
///
/// This could be narrowed. Where one key's columns are a subset of another's
/// and the wider key's parent carries a foreign key of its own covering the
/// shared column and pointing at the same place, taking both from the wider
/// parent is provably consistent — the value came out of that parent's pool
/// in the first place. That is a real static check and it is not written,
/// because this shape is 27 tables in 2,586 and the last two levers of that
/// size in this project were worth two tables and three. Written down so it is
/// a decision rather than an oversight.
fn entangled_foreign_keys(table: &Table, schema: &Schema) -> Option<(String, String, String)> {
    for (index, first) in table.foreign_keys.iter().enumerate() {
        for second in &table.foreign_keys[index + 1..] {
            let duplicate = first.columns == second.columns
                && first.references == second.references
                && first.referenced_columns == second.referenced_columns;
            if duplicate || supplied_by(first, second, schema) || supplied_by(second, first, schema)
            {
                continue;
            }
            if let Some(column) = first.columns.iter().find(|c| second.columns.contains(c)) {
                return Some((first.name.clone(), second.name.clone(), column.clone()));
            }
        }
    }
    None
}

/// Whether taking `narrow`'s columns from `wide`'s parent row satisfies
/// `narrow` too — provably, not probably.
///
/// This is the multi-tenant shape, and it is most of what real SaaS schemas
/// look like. Zitadel's `projects` carries both `(instance_id) -> instances(id)`
/// and `(instance_id, org_id) -> organizations(instance_id, id)`, and the two
/// keys share `instance_id`. Drawing them independently is what produced a
/// pair no parent held; drawing both from the organization row is *correct*,
/// and here is why: `organizations.instance_id` is itself a foreign key to
/// `instances.id`, so the value in that row came out of the instances pool
/// and satisfies the narrow key by construction.
///
/// So the condition is exactly that. Every column of `narrow` must appear in
/// `wide`, and the parent of `wide` must carry one foreign key that maps the
/// corresponding columns onto the same table and the same columns `narrow`
/// points at. One key rather than several, because `narrow` needs its columns
/// to have come from a single row over there, which is the same reason a
/// composite key is drawn one parent row at a time in the first place.
fn supplied_by(narrow: &ForeignKey, wide: &ForeignKey, schema: &Schema) -> bool {
    if narrow.columns.len() > wide.columns.len() {
        return false;
    }
    let Some(parent) = schema.tables.get(&wide.references) else {
        return false;
    };
    // Where each of `narrow`'s columns lands in `wide`'s parent.
    let mut over_there = Vec::with_capacity(narrow.columns.len());
    for mine in &narrow.columns {
        let Some(at) = wide.columns.iter().position(|c| c == mine) else {
            return false;
        };
        let Some(theirs) = wide.referenced_columns.get(at) else {
            return false;
        };
        over_there.push(theirs.clone());
    }
    parent.foreign_keys.iter().any(|onward| {
        onward.references == narrow.references
            && onward.columns == over_there
            && onward.referenced_columns == narrow.referenced_columns
    })
}

/// Where an at-least-one-non-null group has no column that can hold a value.
///
/// Every column of the group is a foreign key, and every one of those parents
/// is refused or was never read — so all of them are written NULL and the
/// constraint has nothing to be satisfied by. Returns the last such parent, to
/// name in the refusal.
///
/// One column that is not a foreign key is enough to make the group fine,
/// because the generator always produces a value for one of those.
fn every_choice_is_blocked(
    table: &Table,
    schema: &Schema,
    refused: &[TableId],
) -> Option<(TableId, String)> {
    for check in &table.checks {
        let crate::checks::Meaning::AtLeastOneNonNull { columns } =
            crate::checks::interpret(&check.definition)
        else {
            continue;
        };
        let mut blocking = None;
        for column in &columns {
            let Some(fk) = table
                .foreign_keys
                .iter()
                .find(|fk| fk.columns.contains(column))
            else {
                // Not a foreign key, so it certainly gets a value.
                blocking = None;
                break;
            };
            if !refused.contains(&fk.references) && schema.tables.contains_key(&fk.references) {
                // This parent is fillable, so this column will hold something.
                blocking = None;
                break;
            }
            blocking = Some((fk.references.clone(), fk.name.clone()));
        }
        if blocking.is_some() {
            return blocking;
        }
    }
    None
}

/// Whether this foreign key holds the one value a `num_nonnulls(...) = 1`
/// constraint on the table demands.
fn fk_carries_the_only_value(table: &Table, fk: &crate::schema::ForeignKey) -> bool {
    table.checks.iter().any(|check| {
        let crate::checks::Meaning::ExactlyOneNonNull { columns } =
            crate::checks::interpret(&check.definition)
        else {
            return false;
        };
        crate::generate::filled_column(table, &columns)
            .is_some_and(|chosen| fk.columns.contains(&chosen))
    })
}

/// Decide about every table.
///
/// Two passes, because refusal is contagious: a table whose required foreign
/// key points at a refused table cannot be filled either, and that has to
/// propagate all the way down the graph. Filling it anyway would produce rows
/// referencing nothing, which is precisely the silent-wrongness this exists to
/// avoid.
pub fn classify(schema: &Schema, order: &Order) -> Verdict {
    let mut refusals: Vec<(TableId, Vec<Refusal>)> = Vec::new();

    for (id, table) in &schema.tables {
        let found = direct_refusals(table, schema, order);
        if !found.is_empty() {
            refusals.push((id.clone(), found));
        }
    }

    // Propagate. Repeat until nothing new is refused, because a chain of
    // dependencies is arbitrarily long and one pass would only catch the
    // tables sitting directly on top of a refusal.
    loop {
        let refused_ids: Vec<TableId> = refusals.iter().map(|(t, _)| t.clone()).collect();
        let mut added = false;

        for (id, table) in &schema.tables {
            if refused_ids.contains(id) {
                continue;
            }
            // `(a IS NOT NULL) OR (b IS NOT NULL)` needs only one of them to
            // hold a value, but if every one is a foreign key and every one
            // of those parents is refused, they all come out NULL and none
            // does. Weaker than the exactly-one rule below and checked over
            // the whole group rather than one key at a time, because one
            // survivor is enough.
            if let Some(blocking) = every_choice_is_blocked(table, schema, &refused_ids) {
                refusals.push((
                    id.clone(),
                    vec![Refusal::DependsOnRefused {
                        table: blocking.0,
                        constraint: blocking.1,
                    }],
                ));
                added = true;
                continue;
            }

            for fk in &table.foreign_keys {
                if fk.references == *id {
                    continue;
                }
                // The column chosen to satisfy `num_nonnulls(...) = 1` has to
                // be one that certainly gets a value. If it is a foreign key
                // whose parent is refused there is no pool to draw from, it
                // comes out NULL, and the count is zero rather than one.
                //
                // This has to be asked *before* the nullable skip below, not
                // after. Every column of a `num_nonnulls` group is nullable by
                // definition — that is what makes the constraint expressible —
                // so the skip swallowed this rule entirely and it never ran
                // once. It passed anyway while few parents were refused, which
                // is the whole trouble with a test agreeing for the wrong
                // reason: reading unique indexes refused far more tables, and
                // the CHECK violations it was written to prevent came straight
                // back.
                if fk_carries_the_only_value(table, fk)
                    && (refused_ids.contains(&fk.references)
                        || !schema.tables.contains_key(&fk.references))
                {
                    refusals.push((
                        id.clone(),
                        vec![Refusal::DependsOnRefused {
                            table: fk.references.clone(),
                            constraint: fk.name.clone(),
                        }],
                    ));
                    added = true;
                    break;
                }

                // A nullable key is not a dependency that can block: the row
                // can simply be written without it.
                if fk.is_optional(table) {
                    continue;
                }

                // A parent that was never read is not a free pass either. It
                // may be in a schema nobody asked for, or a partitioned table
                // introspection skips; either way there is no pool to draw
                // from, and the emitter's only remaining move is to write NULL
                // into a column that forbids it. That was 33 of the 43
                // statements the real-schema corpus had rejected.
                let reason = if refused_ids.contains(&fk.references) {
                    Refusal::DependsOnRefused {
                        table: fk.references.clone(),
                        constraint: fk.name.clone(),
                    }
                } else if !schema.tables.contains_key(&fk.references) {
                    Refusal::DependsOnUnread {
                        table: fk.references.clone(),
                        constraint: fk.name.clone(),
                    }
                } else {
                    continue;
                };
                refusals.push((id.clone(), vec![reason]));
                added = true;
                break;
            }
        }
        if !added {
            break;
        }
    }

    refusals.sort_by(|a, b| a.0.cmp(&b.0));
    let refused_ids: Vec<TableId> = refusals.iter().map(|(t, _)| t.clone()).collect();

    // Insert order, minus anything refused. Order is preserved rather than
    // re-sorted: it is already topological, and re-sorting would break it.
    let fillable: Vec<TableId> = order
        .tables
        .iter()
        .filter(|id| !refused_ids.contains(id))
        .cloned()
        .collect();

    // What the order decided about cycles, carried forward for the emitter.
    let mut deferred_constraints = false;
    let mut deferred_repairs = Vec::new();
    for cycle in &order.cycles {
        match &cycle.strategy {
            crate::graph::CycleStrategy::Deferred { .. } => deferred_constraints = true,
            crate::graph::CycleStrategy::NullThenUpdate { broken } => {
                for (table, constraint) in broken {
                    if !refused_ids.contains(table) {
                        deferred_repairs.push((table.clone(), constraint.clone()));
                    }
                }
            }
            crate::graph::CycleStrategy::Impossible { .. } => {}
        }
    }

    Verdict {
        fillable,
        refused: refusals,
        deferred_constraints,
        deferred_repairs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph;
    use crate::schema::{CheckConstraint, Column, ColumnType, ForeignKey, Table};

    fn col(name: &str, type_: ColumnType, nullable: bool) -> Column {
        Column {
            name: name.into(),
            type_,
            nullable,
            has_default: false,
            default_is_sequence: false,
            is_generated: false,
            position: 1,
        }
    }

    fn table(name: &str, columns: Vec<Column>) -> Table {
        Table {
            id: TableId::new("public", name),
            columns,
            foreign_keys: vec![],
            unique_keys: vec![],
            checks: vec![],
        }
    }

    fn schema_of(tables: Vec<Table>) -> Schema {
        let mut s = Schema::default();
        for t in tables {
            s.tables.insert(t.id.clone(), t);
        }
        s
    }

    fn verdict_for(s: &Schema) -> Verdict {
        classify(s, &graph::order(s))
    }

    fn int() -> ColumnType {
        ColumnType::Integer { bytes: 4 }
    }

    #[test]
    fn a_plain_table_is_fillable() {
        let s = schema_of(vec![table(
            "users",
            vec![col("name", ColumnType::Text { max_length: None }, false)],
        )]);
        let v = verdict_for(&s);
        assert_eq!(v.fillable.len(), 1);
        assert!(v.refused.is_empty());
        assert_eq!(v.reach(), 1.0);
    }

    #[test]
    fn a_check_constraint_refuses_the_table_and_quotes_the_rule() {
        // The whole doctrine: read it, quote it, do not attempt it.
        let mut t = table(
            "invoices",
            vec![col(
                "total",
                ColumnType::Numeric {
                    precision: None,
                    scale: None,
                },
                false,
            )],
        );
        t.checks.push(CheckConstraint {
            name: "invoices_total_positive".into(),
            definition: "CHECK ((num_nonnulls(a, b) = 1))".into(),
        });
        let v = verdict_for(&schema_of(vec![t]));

        assert!(v.fillable.is_empty());
        let (_, reasons) = &v.refused[0];
        let text = reasons[0].explain();
        assert!(text.contains("invoices_total_positive"), "{text}");
        assert!(text.contains("num_nonnulls"), "{text}");
    }

    #[test]
    fn an_unsupported_type_refuses_only_when_a_row_must_supply_it() {
        // A nullable exotic column costs nothing: leave it out.
        let nullable = table(
            "places",
            vec![
                col("name", ColumnType::Text { max_length: None }, false),
                col(
                    "shape",
                    ColumnType::Unsupported {
                        name: "geometry".into(),
                    },
                    true,
                ),
            ],
        );
        assert!(verdict_for(&schema_of(vec![nullable])).refused.is_empty());

        let required = table(
            "places",
            vec![col(
                "shape",
                ColumnType::Unsupported {
                    name: "geometry".into(),
                },
                false,
            )],
        );
        let v = verdict_for(&schema_of(vec![required]));
        assert!(v.refused[0].1[0].explain().contains("geometry"));
    }

    #[test]
    fn a_defaulted_column_of_an_unknown_type_is_left_to_the_database() {
        let mut t = table(
            "events",
            vec![col(
                "payload",
                ColumnType::Unsupported {
                    name: "tsvector".into(),
                },
                false,
            )],
        );
        t.columns[0].has_default = true;
        assert!(verdict_for(&schema_of(vec![t])).refused.is_empty());
    }

    #[test]
    fn refusal_travels_down_the_dependency_chain() {
        // orders → invoices(refused), and items → orders. Filling `items`
        // would produce rows pointing at orders that were never written.
        let mut invoices = table("invoices", vec![col("total", int(), false)]);
        invoices.checks.push(CheckConstraint {
            name: "positive".into(),
            definition: "CHECK ((num_nonnulls(a, b) = 1))".into(),
        });

        let mut orders = table("orders", vec![col("invoice_id", int(), false)]);
        orders.foreign_keys.push(ForeignKey {
            name: "o_inv".into(),
            columns: vec!["invoice_id".into()],
            references: TableId::new("public", "invoices"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });

        let mut items = table("items", vec![col("order_id", int(), false)]);
        items.foreign_keys.push(ForeignKey {
            name: "i_ord".into(),
            columns: vec!["order_id".into()],
            references: TableId::new("public", "orders"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });

        let v = verdict_for(&schema_of(vec![invoices, orders, items]));
        assert!(v.fillable.is_empty(), "{:?}", v.fillable);
        assert_eq!(v.refused.len(), 3);
        let items_reason = &v.refused.iter().find(|(t, _)| t.name == "items").unwrap().1[0];
        assert!(items_reason.explain().contains("orders"));
    }

    #[test]
    fn a_nullable_foreign_key_to_a_refused_table_does_not_refuse_the_child() {
        // The row can simply be written without the reference, so nothing is
        // lost by filling it.
        let mut invoices = table("invoices", vec![col("total", int(), false)]);
        invoices.checks.push(CheckConstraint {
            name: "positive".into(),
            definition: "CHECK ((num_nonnulls(a, b) = 1))".into(),
        });

        let mut orders = table("orders", vec![col("invoice_id", int(), true)]);
        orders.foreign_keys.push(ForeignKey {
            name: "o_inv".into(),
            columns: vec!["invoice_id".into()],
            references: TableId::new("public", "invoices"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });

        let v = verdict_for(&schema_of(vec![invoices, orders]));
        assert_eq!(v.fillable.len(), 1);
        assert_eq!(v.fillable[0].name, "orders");
        assert_eq!(v.reach(), 0.5);
    }

    #[test]
    fn fillable_tables_stay_in_insert_order() {
        // Filtering out a refusal must not disturb the topological order of
        // what remains, or children get written before their parents.
        let users = table(
            "users",
            vec![col("name", ColumnType::Text { max_length: None }, false)],
        );
        let mut orders = table("orders", vec![col("user_id", int(), false)]);
        orders.foreign_keys.push(ForeignKey {
            name: "o_u".into(),
            columns: vec!["user_id".into()],
            references: TableId::new("public", "users"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        let v = verdict_for(&schema_of(vec![orders, users]));
        let names: Vec<&str> = v.fillable.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["users", "orders"]);
    }

    #[test]
    fn a_required_key_to_a_table_never_read_refuses_rather_than_writing_null() {
        // The parent is in a schema nobody asked for, or is a partitioned
        // table introspection skips. Either way there is no pool, and the old
        // answer — write NULL and hope — was 28 of the corpus's rejections.
        let mut orders = table("orders", vec![col("user_id", int(), false)]);
        orders.foreign_keys.push(ForeignKey {
            name: "o_u".into(),
            columns: vec!["user_id".into()],
            references: TableId::new("other", "users"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        let v = verdict_for(&schema_of(vec![orders]));
        assert!(v.fillable.is_empty());
        let text = v.refused[0].1[0].explain();
        assert!(text.contains("other.users"), "{text}");
        assert!(text.contains("not read"), "{text}");
    }

    #[test]
    fn a_nullable_key_to_a_table_never_read_is_still_fine() {
        // Nothing is lost: the row is written without the reference. Refusing
        // here would drop a great many ordinary tables for no gain.
        let mut orders = table("orders", vec![col("user_id", int(), true)]);
        orders.foreign_keys.push(ForeignKey {
            name: "o_u".into(),
            columns: vec!["user_id".into()],
            references: TableId::new("other", "users"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        assert_eq!(verdict_for(&schema_of(vec![orders])).fillable.len(), 1);
    }

    #[test]
    fn a_check_that_forbids_null_makes_a_nullable_key_required() {
        // GitLab adds not-nulls as CHECK constraints to avoid rewriting the
        // table. The column is nullable in the catalogue and not null in fact,
        // and reading only the catalogue produced five CHECK violations.
        let mut orders = table("orders", vec![col("user_id", int(), true)]);
        orders.foreign_keys.push(ForeignKey {
            name: "o_u".into(),
            columns: vec!["user_id".into()],
            references: TableId::new("other", "users"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        });
        orders.checks.push(CheckConstraint {
            name: "check_abc123".into(),
            definition: "CHECK ((user_id IS NOT NULL))".into(),
        });
        let v = verdict_for(&schema_of(vec![orders]));
        assert!(
            v.fillable.is_empty(),
            "a CHECK said this column is not null"
        );
        assert!(v.refused[0].1[0].explain().contains("not read"));
    }

    #[test]
    fn reach_is_zero_for_an_empty_schema_rather_than_a_division_by_zero() {
        assert_eq!(verdict_for(&Schema::default()).reach(), 0.0);
    }
}
