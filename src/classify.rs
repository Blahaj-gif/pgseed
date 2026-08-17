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
use crate::schema::{Schema, Table, TableId};

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

/// Reasons a table cannot be filled *on its own terms*, before anything about
/// what it depends on is considered.
fn direct_refusals(table: &Table, order: &Order) -> Vec<Refusal> {
    let mut out = Vec::new();

    // A CHECK is read and matched against a closed set of exact shapes. Two
    // of them — a length limit, and a NOT NULL written as a CHECK — this
    // already satisfies by construction, and measured against real schemas
    // they are 81% of all the CHECK constraints there are. Everything else is
    // refused, unexamined and unapproximated. See `checks`.
    for check in &table.checks {
        use crate::checks::Meaning;
        let satisfiable = match crate::checks::interpret(&check.definition) {
            // Already true by construction, or a limit the generator honours.
            Meaning::LengthLimit { column, .. }
            | Meaning::NotNull { column }
            | Meaning::ByteLength { column, .. }
            | Meaning::LowerBound { column, .. }
            | Meaning::Lowercase { column } => table.column(&column).is_some(),
            // An obligation rather than a permission: satisfied by writing
            // NULL, and therefore only satisfiable if the column may BE null.
            // A NOT NULL column carrying `(col IS NULL) OR ...` genuinely
            // cannot take this way out, and accepting it would produce rows
            // that violate the constraint the moment they are written.
            Meaning::MustBeNull { column } => {
                table.column(&column).is_some_and(|c| c.nullable)
            }
            Meaning::Unknown => false,
        };
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

    if let Some(reason) = order.reason_for(&table.id) {
        out.push(Refusal::UnbreakableCycle { reason: reason.to_string() });
    }

    out
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
        let found = direct_refusals(table, order);
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
            for fk in &table.foreign_keys {
                // A nullable key is not a dependency that can block: the row
                // can simply be written without it.
                if fk.is_optional(table) || fk.references == *id {
                    continue;
                }
                if refused_ids.contains(&fk.references) {
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
            crate::graph::CycleStrategy::NullThenUpdate { table, constraint } => {
                if !refused_ids.contains(table) {
                    deferred_repairs.push((table.clone(), constraint.clone()));
                }
            }
            crate::graph::CycleStrategy::Impossible { .. } => {}
        }
    }

    Verdict { fillable, refused: refusals, deferred_constraints, deferred_repairs }
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
        let s = schema_of(vec![table("users", vec![col("name", ColumnType::Text { max_length: None }, false)])]);
        let v = verdict_for(&s);
        assert_eq!(v.fillable.len(), 1);
        assert!(v.refused.is_empty());
        assert_eq!(v.reach(), 1.0);
    }

    #[test]
    fn a_check_constraint_refuses_the_table_and_quotes_the_rule() {
        // The whole doctrine: read it, quote it, do not attempt it.
        let mut t = table("invoices", vec![col("total", ColumnType::Numeric { precision: None, scale: None }, false)]);
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
        let nullable = table("places", vec![
            col("name", ColumnType::Text { max_length: None }, false),
            col("shape", ColumnType::Unsupported { name: "geometry".into() }, true),
        ]);
        assert!(verdict_for(&schema_of(vec![nullable])).refused.is_empty());

        let required = table("places", vec![
            col("shape", ColumnType::Unsupported { name: "geometry".into() }, false),
        ]);
        let v = verdict_for(&schema_of(vec![required]));
        assert!(v.refused[0].1[0].explain().contains("geometry"));
    }

    #[test]
    fn a_defaulted_column_of_an_unknown_type_is_left_to_the_database() {
        let mut t = table("events", vec![col("payload", ColumnType::Unsupported { name: "tsvector".into() }, false)]);
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
        let users = table("users", vec![col("name", ColumnType::Text { max_length: None }, false)]);
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
    fn reach_is_zero_for_an_empty_schema_rather_than_a_division_by_zero() {
        assert_eq!(verdict_for(&Schema::default()).reach(), 0.0);
    }
}
