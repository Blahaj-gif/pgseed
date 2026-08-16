//! The order rows have to be inserted in, and what to do when there isn't one.
//!
//! A foreign key is a promise that the row it points at already exists, so the
//! tables form a dependency graph and the insert order is a topological sort
//! of it. That much is textbook. The part that decides whether real schemas
//! work is what happens when the graph has a cycle, and real schemas have
//! cycles constantly — `users.default_org_id → orgs` and `orgs.owner_id →
//! users` is an ordinary way to model a product.
//!
//! Three ways out, in the order they are preferred:
//!
//!   1. **A nullable key.** Insert the row without the reference, then fill it
//!      in with an UPDATE. Costs one extra statement and nothing else.
//!   2. **A deferrable constraint.** Postgres will hold the check until commit,
//!      so both rows can be inserted in either order inside one transaction.
//!   3. **Neither.** The cycle cannot be broken without altering the schema,
//!      so the tables in it are refused and the constraint that would need to
//!      change is named.
//!
//! The third is a real outcome, not a failure to try hard enough. A cycle of
//! NOT NULL, non-deferrable keys genuinely cannot be populated by any sequence
//! of single-row inserts.

use std::collections::{BTreeMap, BTreeSet};

use crate::schema::{Schema, TableId};

/// How a cycle gets broken, or why it cannot be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleStrategy {
    /// Insert without this key, then UPDATE it. Names the table and the
    /// constraint whose columns are left null on the first pass.
    NullThenUpdate { table: TableId, constraint: String },
    /// Every constraint in the cycle can be deferred, so one transaction with
    /// `SET CONSTRAINTS ALL DEFERRED` is enough.
    Deferred { constraints: Vec<String> },
    /// No way through. Carries the sentence a person reads.
    Impossible { reason: String },
}

/// A group of tables that must be handled together because they reference each
/// other. Most "cycles" are a single table with a self-reference.
#[derive(Debug, Clone)]
pub struct Cycle {
    pub tables: Vec<TableId>,
    pub strategy: CycleStrategy,
}

#[derive(Debug, Clone)]
pub struct Order {
    /// Tables in an order that satisfies every foreign key, given the
    /// strategies below.
    pub tables: Vec<TableId>,
    /// The cycles found, and what was decided about each.
    pub cycles: Vec<Cycle>,
}

impl Order {
    /// Tables that cannot be filled because a cycle they sit in is unbreakable.
    pub fn blocked(&self) -> BTreeSet<TableId> {
        let mut out = BTreeSet::new();
        for cycle in &self.cycles {
            if matches!(cycle.strategy, CycleStrategy::Impossible { .. }) {
                out.extend(cycle.tables.iter().cloned());
            }
        }
        out
    }

    pub fn reason_for(&self, table: &TableId) -> Option<&str> {
        self.cycles.iter().find_map(|c| match &c.strategy {
            CycleStrategy::Impossible { reason } if c.tables.contains(table) => {
                Some(reason.as_str())
            }
            _ => None,
        })
    }
}

/// Dependencies of each table: who it must be inserted after.
///
/// Self-references are excluded here and handled as their own cycle, because a
/// table cannot be ordered before itself and treating it as an ordinary edge
/// would make every self-referencing table look like an unbreakable loop.
fn dependencies(schema: &Schema) -> BTreeMap<TableId, BTreeSet<TableId>> {
    let mut out: BTreeMap<TableId, BTreeSet<TableId>> = BTreeMap::new();
    for (id, table) in &schema.tables {
        let entry = out.entry(id.clone()).or_default();
        for fk in &table.foreign_keys {
            // A key pointing outside the schema being read is not a dependency
            // this run can satisfy or order around; the rows are assumed to be
            // there already.
            if fk.references != *id && schema.tables.contains_key(&fk.references) {
                entry.insert(fk.references.clone());
            }
        }
    }
    out
}

/// Work out the insert order, and decide what to do about every cycle.
///
/// Ties are broken by name rather than by whatever order the map iterated in.
/// That is not tidiness: an unstable order means a different plan on every run,
/// and reproducibility is a property this tool sells.
pub fn order(schema: &Schema) -> Order {
    let mut pending = dependencies(schema);
    let mut done: BTreeSet<TableId> = BTreeSet::new();
    let mut sorted: Vec<TableId> = Vec::new();
    let mut cycles: Vec<Cycle> = Vec::new();

    // Self-references first: they are cycles of one and never block ordering.
    for (id, table) in &schema.tables {
        for fk in &table.foreign_keys {
            if fk.references == *id {
                cycles.push(Cycle {
                    tables: vec![id.clone()],
                    strategy: strategy_for(schema, &[id.clone()]),
                });
                break;
            }
        }
    }

    loop {
        // Everything whose dependencies are all satisfied, in name order.
        let ready: Vec<TableId> = pending
            .iter()
            .filter(|(_, deps)| deps.iter().all(|d| done.contains(d)))
            .map(|(id, _)| id.clone())
            .collect();

        if !ready.is_empty() {
            for id in ready {
                pending.remove(&id);
                done.insert(id.clone());
                sorted.push(id);
            }
            continue;
        }
        if pending.is_empty() {
            break;
        }

        // Nothing is ready and tables remain: everything left is in, or behind,
        // a cycle. Take one strongly-connected group and decide about it.
        let group = smallest_cycle(&pending);
        let strategy = strategy_for(schema, &group);
        let blocked = matches!(strategy, CycleStrategy::Impossible { .. });
        cycles.push(Cycle { tables: group.clone(), strategy });

        // Either way the group is now placed: a broken cycle inserts in name
        // order and repairs itself afterwards, and an unbreakable one is
        // recorded so that whatever depends on it is refused too rather than
        // silently dropped.
        for id in group {
            pending.remove(&id);
            done.insert(id.clone());
            if !blocked {
                sorted.push(id);
            }
        }
    }

    Order { tables: sorted, cycles }
}

/// One cycle out of the remaining graph.
///
/// Follows dependencies from the first table until a table repeats, which is
/// enough to find *a* cycle. Not the shortest, and it does not need to be:
/// every table in the returned group genuinely participates, which is what the
/// strategy decision requires.
fn smallest_cycle(pending: &BTreeMap<TableId, BTreeSet<TableId>>) -> Vec<TableId> {
    let start = pending.keys().next().expect("called with a non-empty map");
    let mut path: Vec<TableId> = vec![start.clone()];
    let mut seen: BTreeSet<TableId> = BTreeSet::new();
    seen.insert(start.clone());

    loop {
        let current = path.last().expect("path is never empty");
        let next = pending
            .get(current)
            .and_then(|deps| deps.iter().find(|d| pending.contains_key(d)).cloned());

        match next {
            None => return path,
            Some(id) => {
                if let Some(at) = path.iter().position(|t| *t == id) {
                    // Back to somewhere we have been: the cycle is the tail.
                    return path[at..].to_vec();
                }
                seen.insert(id.clone());
                path.push(id);
            }
        }
    }
}

/// Decide how — or whether — a cycle can be broken.
fn strategy_for(schema: &Schema, group: &[TableId]) -> CycleStrategy {
    // Preferred: some key inside the cycle is nullable, so the row can be
    // inserted without it and updated afterwards.
    for id in group {
        let Some(table) = schema.get(id) else { continue };
        for fk in &table.foreign_keys {
            let points_into_cycle = group.contains(&fk.references);
            if points_into_cycle && fk.is_optional(table) {
                return CycleStrategy::NullThenUpdate {
                    table: id.clone(),
                    constraint: fk.name.clone(),
                };
            }
        }
    }

    // Next best: every constraint in the cycle can be deferred to commit.
    let mut involved: Vec<String> = Vec::new();
    let mut all_deferrable = true;
    for id in group {
        let Some(table) = schema.get(id) else { continue };
        for fk in &table.foreign_keys {
            if group.contains(&fk.references) {
                involved.push(fk.name.clone());
                if !fk.deferrable {
                    all_deferrable = false;
                }
            }
        }
    }
    if all_deferrable && !involved.is_empty() {
        involved.sort();
        return CycleStrategy::Deferred { constraints: involved };
    }

    let names: Vec<String> = group.iter().map(|t| t.to_string()).collect();
    let rigid: Vec<String> = group
        .iter()
        .filter_map(|id| schema.get(id))
        .flat_map(|t| {
            t.foreign_keys
                .iter()
                .filter(|fk| group.contains(&fk.references) && !fk.deferrable)
                .filter(|fk| !fk.is_optional(t))
                .map(|fk| fk.name.clone())
        })
        .collect();

    CycleStrategy::Impossible {
        reason: format!(
            "foreign key cycle between {} — every key in it is NOT NULL and \
             not deferrable, so no order of single-row inserts can satisfy it. \
             Making {} deferrable, or one of its columns nullable, would be \
             enough.",
            names.join(" and "),
            rigid.first().map(String::as_str).unwrap_or("one of them"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ColumnType, ForeignKey, Table};

    fn col(name: &str, nullable: bool) -> Column {
        Column {
            name: name.into(),
            type_: ColumnType::Integer { bytes: 4 },
            nullable,
            has_default: false,
            is_generated: false,
            position: 1,
        }
    }

    fn table(name: &str, columns: Vec<Column>, fks: Vec<ForeignKey>) -> Table {
        Table {
            id: TableId::new("public", name),
            columns,
            foreign_keys: fks,
            unique_keys: vec![],
            checks: vec![],
        }
    }

    fn fk(name: &str, column: &str, to: &str, deferrable: bool) -> ForeignKey {
        ForeignKey {
            name: name.into(),
            columns: vec![column.into()],
            references: TableId::new("public", to),
            referenced_columns: vec!["id".into()],
            deferrable,
        }
    }

    fn schema_of(tables: Vec<Table>) -> Schema {
        let mut s = Schema::default();
        for t in tables {
            s.tables.insert(t.id.clone(), t);
        }
        s
    }

    fn names(order: &Order) -> Vec<String> {
        order.tables.iter().map(|t| t.name.clone()).collect()
    }

    #[test]
    fn parents_are_inserted_before_children() {
        let s = schema_of(vec![
            table("orders", vec![col("user_id", false)], vec![fk("fk", "user_id", "users", false)]),
            table("users", vec![col("id", false)], vec![]),
        ]);
        assert_eq!(names(&order(&s)), vec!["users", "orders"]);
    }

    #[test]
    fn a_chain_is_ordered_all_the_way_down() {
        let s = schema_of(vec![
            table("items", vec![col("order_id", false)], vec![fk("a", "order_id", "orders", false)]),
            table("orders", vec![col("user_id", false)], vec![fk("b", "user_id", "users", false)]),
            table("users", vec![col("id", false)], vec![]),
        ]);
        assert_eq!(names(&order(&s)), vec!["users", "orders", "items"]);
    }

    #[test]
    fn independent_tables_come_out_in_name_order_not_map_order() {
        // Reproducibility: an unstable tie-break means a different plan every
        // run, and this tool sells determinism.
        let s = schema_of(vec![
            table("zebra", vec![col("id", false)], vec![]),
            table("apple", vec![col("id", false)], vec![]),
            table("mango", vec![col("id", false)], vec![]),
        ]);
        assert_eq!(names(&order(&s)), vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn a_self_reference_is_a_cycle_of_one_and_does_not_block_ordering() {
        // `employees.manager_id → employees` is ordinary, and a table cannot be
        // ordered before itself.
        let s = schema_of(vec![table(
            "employees",
            vec![col("manager_id", true)],
            vec![fk("mgr", "manager_id", "employees", false)],
        )]);
        let o = order(&s);
        assert_eq!(names(&o), vec!["employees"]);
        assert_eq!(o.cycles.len(), 1);
        assert!(matches!(o.cycles[0].strategy, CycleStrategy::NullThenUpdate { .. }));
    }

    #[test]
    fn a_two_table_cycle_is_broken_through_the_nullable_side() {
        // users.default_org_id → orgs, orgs.owner_id → users. The nullable one
        // is inserted empty and filled in afterwards.
        let s = schema_of(vec![
            table("users", vec![col("default_org_id", true)],
                  vec![fk("u_org", "default_org_id", "orgs", false)]),
            table("orgs", vec![col("owner_id", false)],
                  vec![fk("o_owner", "owner_id", "users", false)]),
        ]);
        let o = order(&s);
        assert_eq!(o.cycles.len(), 1);
        match &o.cycles[0].strategy {
            CycleStrategy::NullThenUpdate { table, constraint } => {
                assert_eq!(table.name, "users");
                assert_eq!(constraint, "u_org");
            }
            other => panic!("expected a nullable break, got {other:?}"),
        }
        assert_eq!(o.tables.len(), 2);
        assert!(o.blocked().is_empty());
    }

    #[test]
    fn a_cycle_of_deferrable_keys_is_deferred() {
        let s = schema_of(vec![
            table("a", vec![col("b_id", false)], vec![fk("a_b", "b_id", "b", true)]),
            table("b", vec![col("a_id", false)], vec![fk("b_a", "a_id", "a", true)]),
        ]);
        let o = order(&s);
        match &o.cycles[0].strategy {
            CycleStrategy::Deferred { constraints } => {
                assert_eq!(constraints, &vec!["a_b".to_string(), "b_a".to_string()]);
            }
            other => panic!("expected deferral, got {other:?}"),
        }
    }

    #[test]
    fn an_unbreakable_cycle_is_refused_and_says_what_would_fix_it() {
        // Every key NOT NULL and not deferrable: genuinely impossible, and the
        // message has to be actionable rather than an apology.
        let s = schema_of(vec![
            table("a", vec![col("b_id", false)], vec![fk("a_b", "b_id", "b", false)]),
            table("b", vec![col("a_id", false)], vec![fk("b_a", "a_id", "a", false)]),
        ]);
        let o = order(&s);
        let blocked = o.blocked();
        assert_eq!(blocked.len(), 2);
        let reason = o.reason_for(&TableId::new("public", "a")).unwrap();
        assert!(reason.contains("deferrable"), "{reason}");
        assert!(reason.contains("nullable"), "{reason}");
        // A refused table is not in the insert order.
        assert!(o.tables.is_empty());
    }

    #[test]
    fn a_key_pointing_outside_the_read_schema_is_not_a_dependency() {
        // Reading only `public` while a key points at `audit.events` should not
        // invent an ordering constraint on a table this run never saw.
        let s = schema_of(vec![table(
            "orders",
            vec![col("event_id", false)],
            vec![ForeignKey {
                name: "fk".into(),
                columns: vec!["event_id".into()],
                references: TableId::new("audit", "events"),
                referenced_columns: vec!["id".into()],
                deferrable: false,
            }],
        )]);
        let o = order(&s);
        assert_eq!(names(&o), vec!["orders"]);
        assert!(o.cycles.is_empty());
    }

    #[test]
    fn an_empty_schema_orders_to_nothing_without_panicking() {
        let o = order(&Schema::default());
        assert!(o.tables.is_empty() && o.cycles.is_empty());
    }
}
