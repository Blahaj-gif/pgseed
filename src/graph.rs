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
    /// Insert without these keys, then UPDATE them. Names every (table,
    /// constraint) whose columns are left null on the first pass.
    ///
    /// A list rather than a single edge, because removing one edge from a
    /// strongly-connected group does not generally make it acyclic. GitLab's
    /// vulnerability tables are a group of several interlocking loops, and
    /// breaking one of them still left a required key pointing forwards.
    NullThenUpdate { broken: Vec<(TableId, String)> },
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
                    strategy: strategy_for(schema, std::slice::from_ref(id)),
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

        // A broken cycle is inserted in the order that remains once the
        // nulled keys are set aside — not in name order, which is right only
        // by luck. With `b.x` nulled, `b` no longer waits for `a`, but `a`
        // still waits for `b`, and the alphabet has no opinion about that.
        // A deferred cycle genuinely has no required order, and an unbreakable
        // one is recorded so whatever depends on it is refused rather than
        // silently dropped.
        let placement = match cycles.last().map(|c| &c.strategy) {
            Some(CycleStrategy::NullThenUpdate { broken }) => {
                sort_group(schema, &group, broken).unwrap_or_else(|| group.clone())
            }
            _ => group.clone(),
        };
        for id in &group {
            pending.remove(id);
            done.insert(id.clone());
        }
        if !blocked {
            sorted.extend(placement);
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

/// Sort a cycle's tables into an insert order, given the keys being nulled.
///
/// `None` means the group is still cyclic with those keys removed, which is
/// the question the caller is really asking — there is no order, so more has
/// to be broken or another strategy found.
fn sort_group(
    schema: &Schema,
    group: &[TableId],
    broken: &[(TableId, String)],
) -> Option<Vec<TableId>> {
    let mut pending: BTreeMap<TableId, BTreeSet<TableId>> = BTreeMap::new();
    for id in group {
        let Some(table) = schema.get(id) else { continue };
        let mut deps = BTreeSet::new();
        for fk in &table.foreign_keys {
            // A self-reference counts here, unlike in the outer graph. There
            // it is excluded so the table is not blocked on itself forever;
            // here the question is whether a row can be inserted at all, and
            // one that must point at a row of its own table cannot — not
            // until the key has been left null and filled in afterwards.
            let removed = broken.iter().any(|(t, c)| t == id && *c == fk.name);
            if removed || !group.contains(&fk.references) {
                continue;
            }
            deps.insert(fk.references.clone());
        }
        pending.insert(id.clone(), deps);
    }

    let mut done: BTreeSet<TableId> = BTreeSet::new();
    let mut sorted: Vec<TableId> = Vec::new();
    while !pending.is_empty() {
        let ready: Vec<TableId> = pending
            .iter()
            .filter(|(_, deps)| deps.iter().all(|d| done.contains(d)))
            .map(|(id, _)| id.clone())
            .collect();
        if ready.is_empty() {
            return None;
        }
        for id in ready {
            pending.remove(&id);
            done.insert(id.clone());
            sorted.push(id);
        }
    }
    Some(sorted)
}

/// The next nullable key inside the cycle that has not been broken already.
///
/// Deterministic: tables in the order the group holds them, keys in declared
/// order. An unstable choice here means a different insert order per run, and
/// reproducibility is a property this tool sells.
fn next_optional_edge(
    schema: &Schema,
    group: &[TableId],
    broken: &[(TableId, String)],
) -> Option<(TableId, String)> {
    for id in group {
        let table = schema.get(id)?;
        for fk in &table.foreign_keys {
            if !group.contains(&fk.references) || !fk.is_optional(table) {
                continue;
            }
            if broken.iter().any(|(t, c)| t == id && *c == fk.name) {
                continue;
            }
            return Some((id.clone(), fk.name.clone()));
        }
    }
    None
}

/// Decide how — or whether — a cycle can be broken.
fn strategy_for(schema: &Schema, group: &[TableId]) -> CycleStrategy {
    // Preferred: enough keys inside the cycle are nullable that removing them
    // leaves something that can actually be sorted. One is usually enough and
    // used to be assumed sufficient, which was wrong — a strongly-connected
    // group can hold several interlocking loops, and the leftover edges then
    // point forwards at rows that have not been written yet.
    let mut broken: Vec<(TableId, String)> = Vec::new();
    while sort_group(schema, group, &broken).is_none() {
        match next_optional_edge(schema, group, &broken) {
            Some(edge) => broken.push(edge),
            // Out of nullable keys with the group still cyclic. Fall through
            // to deferring, which does not care about order at all.
            None => break,
        }
    }
    if !broken.is_empty() && sort_group(schema, group, &broken).is_some() {
        return CycleStrategy::NullThenUpdate { broken };
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
            default_is_sequence: false,
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
            CycleStrategy::NullThenUpdate { broken } => {
                assert_eq!(broken.len(), 1, "one edge is enough for a two-cycle");
                assert_eq!(broken[0].0.name, "users");
                assert_eq!(broken[0].1, "u_org");
            }
            other => panic!("expected a nullable break, got {other:?}"),
        }
        assert_eq!(o.tables.len(), 2);
        assert!(o.blocked().is_empty());

        // And in the order that remains once `users.default_org_id` is set
        // aside: orgs still requires users, so users goes first.
        let names: Vec<&str> = o.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["users", "orgs"]);
    }

    #[test]
    fn a_group_of_two_loops_breaks_both_and_sorts_what_is_left() {
        // The shape that was wrong on GitLab: `a` and `b` reference each other
        // twice over. Breaking one nullable key leaves the other loop intact,
        // and placing the group alphabetically then writes `a` before `b`
        // while `a` still requires it.
        let s = schema_of(vec![
            table("a", vec![col("b1", true), col("b2", true)],
                  vec![fk("a_b1", "b1", "b", false), fk("a_b2", "b2", "b", false)]),
            table("b", vec![col("a1", true), col("a2", true)],
                  vec![fk("b_a1", "a1", "a", false), fk("b_a2", "a2", "a", false)]),
        ]);
        let o = order(&s);
        match &o.cycles[0].strategy {
            CycleStrategy::NullThenUpdate { broken } => {
                assert_eq!(broken.len(), 2, "one break leaves the group cyclic");
            }
            other => panic!("expected a nullable break, got {other:?}"),
        }
        assert_eq!(o.tables.len(), 2);
    }

    #[test]
    fn a_key_a_check_says_is_not_null_cannot_be_used_to_break_a_cycle() {
        // Nulling it would violate the CHECK on the very first insert.
        use crate::schema::CheckConstraint;
        let mut users = table("users", vec![col("default_org_id", true)],
                              vec![fk("u_org", "default_org_id", "orgs", false)]);
        users.checks.push(CheckConstraint {
            name: "check_org".into(),
            definition: "CHECK ((default_org_id IS NOT NULL))".into(),
        });
        let s = schema_of(vec![
            users,
            table("orgs", vec![col("owner_id", false)],
                  vec![fk("o_owner", "owner_id", "users", false)]),
        ]);
        let o = order(&s);
        assert!(
            matches!(o.cycles[0].strategy, CycleStrategy::Impossible { .. }),
            "got {:?}", o.cycles[0].strategy
        );
        assert_eq!(o.blocked().len(), 2);
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
