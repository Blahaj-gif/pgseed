//! Turning a plan into SQL.
//!
//! The one thing this has to get right that generation alone cannot: a foreign
//! key must name a row that exists. So keys are *pooled* — as each table is
//! written, the values it used for its primary key are kept, and a child
//! picking a parent picks from that pool rather than inventing a number and
//! hoping.
//!
//! Which is also why insert order is load-bearing rather than tidy. A child
//! written before its parent has no pool to draw from, and the only honest
//! thing left would be to refuse it.

use std::collections::BTreeMap;

use crate::classify::Verdict;
use crate::generate::{bounds_for, value, Bounds, Literal};
use crate::schema::{quote_ident, Schema, Table, TableId};

/// Forwards each finished statement to the caller, and remembers whether the
/// caller asked to stop. A tiny shim so the generation code below can go on
/// saying `out.push(...)` and mean "hand this over" rather than "keep this".
struct Emitter<'a> {
    emit: &'a mut dyn FnMut(Written<'_>) -> Took,
    stopped: bool,
}

/// What became of a statement once it was handed over.
///
/// Only `probe` ever says anything but `Kept`, and it is the whole reason this
/// exists: the key pool records what a table's INSERT *would* write, and a
/// child draws its foreign keys from it. If that INSERT was offered to the
/// database and refused, the pool is holding rows that do not exist, and the
/// next child points at them. Four corpus schemas failed exactly that way
/// before this was here, each as a foreign key violation on a table the tool
/// had promised it could fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Took {
    /// It stands. What it wrote is available to children.
    Kept,
    /// It did not happen, so nothing may reference what it would have written.
    Rejected,
    /// Stop here.
    Stop,
}

/// A statement, and the table it is about where there is one.
///
/// The consumer needs the table as well as the text: `probe` has to know which
/// rows it is allowed to lose. Reading the name back out of the SQL would work
/// and would be one more thing that has to stay in step with the emitter, so
/// it is handed over instead.
#[derive(Debug, Clone, Copy)]
pub struct Written<'a> {
    pub sql: &'a str,
    /// `None` for the statements that belong to no single table — deferring
    /// the constraints, and repairing a cycle after the fact.
    pub table: Option<&'a TableId>,
}

impl Emitter<'_> {
    fn push(&mut self, statement: impl AsRef<str>) {
        self.about(None, statement);
    }

    /// Hand a statement over, and say whether what it wrote can be relied on.
    fn about(&mut self, table: Option<&TableId>, statement: impl AsRef<str>) -> bool {
        if self.stopped {
            return false;
        }
        match (self.emit)(Written {
            sql: statement.as_ref(),
            table,
        }) {
            Took::Kept => true,
            Took::Rejected => false,
            Took::Stop => {
                self.stopped = true;
                false
            }
        }
    }

    fn extend(&mut self, statements: impl IntoIterator<Item = String>) {
        for statement in statements {
            self.push(statement);
        }
    }
}

/// The value a table *will* write for one of its columns, worked out without
/// having written it.
///
/// Only for a foreign key that points into a cycle being populated by deferring
/// the constraints to commit. There the child is written before the parent on
/// purpose, so the pool is empty and the usual routes are shut: there is no row
/// to borrow from the database either, because the parent has none yet.
///
/// It works because nothing about generation depends on order. A cell's value
/// comes from `(seed, table, column, row)` and from that table's own bounds and
/// unique keys, all of which are known now. So the parent's key is computed
/// exactly as the parent will compute it, and the two agree because they are
/// the same function of the same arguments.
///
/// `None` where the parent's key is not something this writes — a serial, an
/// identity column, a default. Those are chosen by the database at insert time,
/// and there is no way to know one in advance that does not amount to guessing
/// what a sequence will hand out.
fn planned_key(
    schema: &Schema,
    fk: &crate::schema::ForeignKey,
    referenced: &str,
    row: usize,
    counts: &BTreeMap<TableId, usize>,
    options: &Options,
) -> Option<Literal> {
    let parent = schema.get(&fk.references)?;
    let column = parent.column(referenced)?;
    if column.is_generated || column.has_default {
        return None;
    }
    if !writable(parent).iter().any(|c| c.name == column.name) {
        return None;
    }
    // Wrapped on how many rows the parent is going to hold, so this names one
    // of them rather than one past the end.
    let parent_rows = counts
        .get(&fk.references)
        .copied()
        .unwrap_or_else(|| options.rows.for_table(&fk.references));
    if parent_rows == 0 {
        return None;
    }
    let bounds = bounds_for(parent);
    let varying = crate::volume::variations(parent);
    Some(value(
        options.seed,
        &fk.references,
        column,
        row % parent_rows,
        bounds.get(&column.name).unwrap_or(&Bounds::default()),
        varying.get(&column.name).copied(),
    ))
}

/// Values already written for a table's primary key, so children can point at
/// them. Keyed by column, because a composite key has to be drawn as a *row*
/// rather than column by column — picking `tenant_id` from one parent and
/// `code` from another names a pair that never existed.
type KeyPool = BTreeMap<TableId, Vec<BTreeMap<String, Literal>>>;

pub struct Options {
    pub seed: u64,
    /// Rows per table: one default and any number of per-table overrides, so
    /// `--rows 50 --rows order_items=500` does what it looks like.
    pub rows: crate::filter::RowCounts,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            seed: 1,
            rows: crate::filter::RowCounts::default(),
        }
    }
}

impl Options {
    /// A flat count for every table, which is what most callers want.
    pub fn flat(seed: u64, rows: usize) -> Options {
        Options {
            seed,
            rows: crate::filter::RowCounts::flat(rows),
        }
    }
}

/// Which columns of this table a row has to supply a value for.
///
/// Generated and defaulted columns are left out entirely: naming an identity
/// column is an error, and overriding a default is usually the opposite of
/// what the schema author meant.
fn writable(table: &Table) -> Vec<&crate::schema::Column> {
    table.columns_to_write()
}

/// A foreign key whose parent's key is generated by the database.
///
/// The commonest schema in the world and the one the first version of this got
/// wrong: `users.id` is `GENERATED ALWAYS AS IDENTITY`, so it is never written
/// and never enters the pool, so a child pointing at it found nothing and
/// wrote NULL into a NOT NULL column.
///
/// The value cannot be known when the SQL is written, so the SQL asks for it.
/// A subquery selecting the nth parent row, ordered by the referenced columns
/// so that every column of a composite key lands on the *same* row — the same
/// ORDER BY and the same OFFSET can only name one.
fn borrow_from_database(
    table: &Table,
    column: &crate::schema::Column,
    row: usize,
    schema: &Schema,
) -> Option<Literal> {
    let fk = table
        .foreign_keys
        .iter()
        .find(|fk| fk.columns.contains(&column.name))?;
    let parent = schema.get(&fk.references)?;

    // Only when the parent's key really is out of our hands. If we wrote it
    // ourselves, the pool has it and this must not fire.
    let position = fk.columns.iter().position(|c| *c == column.name)?;
    let referenced = fk.referenced_columns.get(position)?;
    let parent_column = parent.column(referenced)?;
    if !(parent_column.is_generated || parent_column.has_default) {
        return None;
    }

    let order: Vec<String> = fk
        .referenced_columns
        .iter()
        .map(|c| quote_ident(c))
        .collect();
    // The offset wraps on how many rows the parent actually has. It used to be
    // the plain row index, which walks off the end the moment the parent holds
    // fewer rows than this table, and since row counts started respecting
    // what a table can hold, that is no longer rare. Past the end the subquery
    // returns NULL, and a NULL foreign key is either a violation or a lie.
    //
    // Counted in SQL rather than from the plan, so it stays right when the
    // parent already had rows of its own and nothing was truncated. GREATEST
    // keeps an empty parent from dividing by zero; the value is then NULL,
    // which is the truth, and the database says so.
    Some(Literal(format!(
        "(SELECT {} FROM {} ORDER BY {} LIMIT 1 OFFSET \
         ({row} % GREATEST((SELECT count(*) FROM {}), 1)))",
        quote_ident(referenced),
        fk.references.quoted(),
        order.join(", "),
        fk.references.quoted(),
    )))
}

/// The SQL to fill everything the verdict said was fillable, as one string.
///
/// Wrapped in a transaction, because a partial seed is worse than none: it
/// looks like it worked.
pub fn sql(schema: &Schema, verdict: &Verdict, options: &Options) -> String {
    let mut out = Vec::new();
    write_sql(&mut out, schema, verdict, options).expect("writing to a Vec cannot fail");
    String::from_utf8(out).expect("every statement is valid UTF-8")
}

/// Write the SQL out, a statement at a time.
///
/// What the binary uses. `sql` builds the same text in memory and is kept for
/// the tests, where holding a few kilobytes is the convenient thing; this
/// holds one statement, which is what makes a very large schema at a very
/// large row count a question of patience rather than of available memory.
pub fn write_sql(
    writer: &mut dyn std::io::Write,
    schema: &Schema,
    verdict: &Verdict,
    options: &Options,
) -> std::io::Result<()> {
    let mut failure = None;
    writer.write_all(
        b"BEGIN;
",
    )?;
    for_each_statement(schema, verdict, options, &mut |written| match writer
        .write_all(
            b"
",
        )
        .and_then(|()| writer.write_all(written.sql.as_bytes()))
        .and_then(|()| {
            writer.write_all(
                b"
",
            )
        }) {
        Ok(()) => Took::Kept,
        Err(e) => {
            failure = Some(e);
            Took::Stop
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    writer.write_all(
        b"
COMMIT;
",
    )
}

/// Run everything against a database, inside one transaction.
///
/// All or nothing. A seed that stopped halfway leaves a database that looks
/// populated and is not, which is the shape of failure this whole tool is
/// built against.
pub fn apply(
    client: &mut postgres::Client,
    schema: &Schema,
    verdict: &Verdict,
    options: &Options,
) -> Result<usize, postgres::Error> {
    let mut transaction = client.transaction()?;
    let mut count = 0usize;
    let mut failure = None;
    for_each_statement(schema, verdict, options, &mut |written| match transaction
        .batch_execute(written.sql)
    {
        Ok(()) => {
            count += 1;
            Took::Kept
        }
        Err(e) => {
            failure = Some(e);
            Took::Stop
        }
    });
    if let Some(e) = failure {
        // Dropping the transaction rolls it back, so the database is as it
        // was. Returning the error rather than a count says so.
        return Err(e);
    }
    transaction.commit()?;
    Ok(count)
}

/// Every statement needed, in order, without a transaction around them.
///
/// One implementation behind both the SQL text and the direct apply. Two would
/// drift, and the one nobody tested would be the one somebody ran.
pub fn statements(schema: &Schema, verdict: &Verdict, options: &Options) -> Vec<String> {
    let mut out = Vec::new();
    for_each_statement(schema, verdict, options, &mut |written| {
        out.push(written.sql.to_string());
        Took::Kept
    });
    out
}

/// Every statement, handed over one at a time.
///
/// The streaming form, and the one the binary uses. Holding them all costs
/// what they weigh — GitLab at a thousand rows a table is 205 MB — and there
/// is no reason to, since each is finished before the next begins. Return
/// `false` from `emit` to stop early, which is how `apply` gets out on the
/// first statement the database refuses.
pub fn for_each_statement(
    schema: &Schema,
    verdict: &Verdict,
    options: &Options,
    emit: &mut dyn FnMut(Written<'_>) -> Took,
) {
    let mut out = Emitter {
        emit,
        stopped: false,
    };
    let mut pool: KeyPool = BTreeMap::new();

    // A cycle of deferrable keys is populated by holding the checks until
    // commit. Postgres allows that only for constraints declared DEFERRABLE,
    // which is why `graph` decides it and this only says it.
    if verdict.deferred_constraints {
        out.push("SET CONSTRAINTS ALL DEFERRED;");
    }

    // How many rows each table gets: what was asked for, or as many as its
    // constraints leave room for. Not recomputed per table — a cap on a parent
    // caps its children, so this is one pass down the graph.
    let counts = crate::volume::plan(schema, &verdict.fillable, &options.rows);

    for id in &verdict.fillable {
        let Some(table) = schema.get(id) else {
            continue;
        };

        let rows = counts
            .get(id)
            .copied()
            .unwrap_or_else(|| options.rows.for_table(id));
        if rows == 0 {
            // Nothing can be written — a parent came out empty, and inventing
            // a key it does not have is the one thing not on offer.
            continue;
        }

        let columns = writable(table);
        if columns.is_empty() {
            // Every column is generated or defaulted, so the only thing to say
            // is "a row exists".
            for _ in 0..rows {
                let _ = out.about(
                    Some(id),
                    format!("INSERT INTO {} DEFAULT VALUES;", id.quoted()),
                );
            }
            continue;
        }

        let bounds = bounds_for(table);
        let names: Vec<String> = columns.iter().map(|c| quote_ident(&c.name)).collect();
        let mut written: Vec<BTreeMap<String, Literal>> = Vec::new();

        let mut statement = format!(
            "INSERT INTO {} ({}) VALUES\n",
            id.quoted(),
            names.join(", ")
        );

        // Non-empty only for a join table, where the parents have to be walked
        // as digits of one number so the combinations do not repeat.
        let strides = crate::volume::strides(table, &counts);

        // How often each column has to change so no unique key repeats. One
        // means every row; a larger stride makes the column a slower digit of
        // a composite key's odometer.
        let varying = crate::volume::variations(table);

        for row in 0..rows {
            let mut values: Vec<String> = Vec::with_capacity(columns.len());
            let mut this_row: BTreeMap<String, Literal> = BTreeMap::new();

            // Foreign keys first: a column covered by one is drawn from the
            // parent's pool, never generated, or it would point at nothing.
            let mut from_parent: BTreeMap<String, Literal> = BTreeMap::new();
            for fk in &table.foreign_keys {
                let Some(parent_rows) = pool.get(&fk.references) else {
                    continue;
                };
                if parent_rows.is_empty() {
                    continue;
                }
                // One parent row per foreign key, so every column of a
                // composite key comes from the same row.
                let stride = strides.get(&fk.name).copied().unwrap_or(1);
                let chosen = &parent_rows[(row / stride) % parent_rows.len()];
                for (mine, theirs) in fk.columns.iter().zip(&fk.referenced_columns) {
                    if let Some(literal) = chosen.get(theirs) {
                        from_parent.insert(mine.clone(), literal.clone());
                    }
                }
            }

            for column in &columns {
                // An obligation to be null comes before everything, including
                // the foreign key paths below. It used to come after them, so
                // a column a CHECK required to be null was handed its parent's
                // key anyway — which is how widening the closed set to
                // `num_nonnulls(a, b) = 1` produced rows with two of them.
                let literal = if bounds
                    .get(&column.name)
                    .is_some_and(|b: &Bounds| b.must_be_null)
                {
                    Literal::null()
                } else if let Some(borrowed) = from_parent.get(&column.name) {
                    borrowed.clone()
                } else if let Some(lookup) = borrow_from_database(table, column, row, schema) {
                    lookup
                } else if let Some(planned) = table
                    .foreign_keys
                    .iter()
                    .filter(|fk| fk.columns.contains(&column.name))
                    // Only where the check really is deferred to commit. A
                    // cycle broken by nulling instead has its keys checked the
                    // instant the row lands, so naming a row the parent has not
                    // written yet is a violation rather than a forward
                    // reference — which is exactly what a ring of ten tables
                    // demonstrated the first time this was let loose on all of
                    // them.
                    .filter(|fk| fk.deferrable && verdict.deferred_constraints)
                    .find_map(|fk| {
                        let at = fk.columns.iter().position(|c| *c == column.name)?;
                        let referenced = fk.referenced_columns.get(at)?;
                        planned_key(schema, fk, referenced, row, &counts, options)
                    })
                {
                    // A cycle whose constraints are deferred to commit: the
                    // parent has not been written yet and will be, with exactly
                    // this key. Writing NULL here instead is what a NOT NULL
                    // column in a deferrable ring used to get, and the database
                    // rejected the row — a promise `classify` had made and the
                    // generator could not keep.
                    planned
                } else if table
                    .foreign_keys
                    .iter()
                    .any(|fk| fk.columns.contains(&column.name))
                {
                    // A key whose parent has no pool — because the parent was
                    // refused, or is outside this schema. Nullable is fine;
                    // anything else has no honest value and the classifier
                    // should already have refused the table.
                    Literal::null()
                } else if !column.type_.is_generatable() {
                    // No value can be produced. `classify` refuses the table
                    // when such a column is required, so reaching here means
                    // it is nullable, and NULL is the honest answer. The old
                    // one was the literal `DEFAULT`, which is legal as a bare
                    // column value and a syntax error inside `ARRAY[...]`.
                    Literal::null()
                } else {
                    value(
                        options.seed,
                        id,
                        column,
                        row,
                        bounds.get(&column.name).unwrap_or(&Bounds::default()),
                        varying.get(&column.name).copied(),
                    )
                };
                this_row.insert(column.name.clone(), literal.clone());
                values.push(literal.0);
            }

            statement.push_str(&format!(
                "  ({}){}\n",
                values.join(", "),
                if row + 1 == rows { ";" } else { "," }
            ));
            written.push(this_row);
        }

        // Only what the consumer kept goes into the pool. A child drawing from
        // it is naming a row by its key, and a key from a statement that was
        // rolled back names nothing.
        if out.about(Some(id), statement) {
            pool.insert(id.clone(), written);
        }
    }

    out.extend(repair_cycles(schema, verdict));
}

/// Fill in the keys that were left NULL to break a cycle.
///
/// `graph` breaks a cycle by inserting the row without its reference. Without
/// this the column stays NULL forever, which is *valid* and useless: a
/// `manager_id` that is null on every row has not modelled anything.
///
/// The root is found by ordering rather than by `min()`, because `min()` has
/// no overload for `uuid` and Lago keys every one of its 137 tables that way.
/// `ORDER BY ... LIMIT 1` is the same row and works for anything that can be
/// compared at all, which is anything that can be a key.
///
/// Every row but one is pointed at the lowest-keyed other row — stable,
/// reproducible, never self-referential. The one left out is the root: a table
/// where every row has a parent is a closed loop, which is exactly what the
/// constraint existed to prevent.
fn repair_cycles(schema: &Schema, verdict: &Verdict) -> Vec<String> {
    let mut out = Vec::new();
    for (table_id, constraint) in &verdict.deferred_repairs {
        let Some(table) = schema.get(table_id) else {
            continue;
        };
        let Some(fk) = table.foreign_keys.iter().find(|f| f.name == *constraint) else {
            continue;
        };
        // A composite repair needs its parent chosen as a row rather than
        // column by column, and is left alone rather than done badly.
        if fk.columns.len() != 1 || fk.referenced_columns.len() != 1 {
            continue;
        }
        let Some(own) = table.primary_key().filter(|k| k.columns.len() == 1) else {
            continue;
        };

        out.push(format!(
            "UPDATE {child} AS c SET {column} = (SELECT p.{parent_column} \
             FROM {parent} AS p WHERE p.{parent_column} <> c.{own_key} \
             ORDER BY p.{parent_column} LIMIT 1) \
             WHERE c.{own_key} <> (SELECT x.{own_key} FROM {child} AS x ORDER BY x.{own_key} LIMIT 1);",
            child = table_id.quoted(),
            column = quote_ident(&fk.columns[0]),
            parent = fk.references.quoted(),
            parent_column = quote_ident(&fk.referenced_columns[0]),
            own_key = quote_ident(&own.columns[0]),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ColumnType, ForeignKey, Table, UniqueKey};
    use crate::{classify, graph};

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

    fn schema_of(tables: Vec<Table>) -> Schema {
        let mut s = Schema::default();
        for t in tables {
            s.tables.insert(t.id.clone(), t);
        }
        s
    }

    fn render(s: &Schema, rows: usize) -> String {
        let order = graph::order(s);
        let verdict = classify::classify(s, &order);
        sql(s, &verdict, &Options::flat(1, rows))
    }

    fn users_and_orders() -> Schema {
        let users = Table {
            id: TableId::new("public", "users"),
            columns: vec![
                col("id", ColumnType::Integer { bytes: 4 }, false),
                col("email", ColumnType::Text { max_length: None }, false),
            ],
            foreign_keys: vec![],
            unique_keys: vec![
                UniqueKey {
                    name: "users_pkey".into(),
                    columns: vec!["id".into()],
                    is_primary: true,
                },
                UniqueKey {
                    name: "users_email_key".into(),
                    columns: vec!["email".into()],
                    is_primary: false,
                },
            ],
            checks: vec![],
        };
        let orders = Table {
            id: TableId::new("public", "orders"),
            columns: vec![
                col("id", ColumnType::Integer { bytes: 4 }, false),
                col("user_id", ColumnType::Integer { bytes: 4 }, false),
            ],
            foreign_keys: vec![ForeignKey {
                name: "orders_user_fk".into(),
                columns: vec!["user_id".into()],
                references: TableId::new("public", "users"),
                referenced_columns: vec!["id".into()],
                deferrable: false,
            }],
            unique_keys: vec![UniqueKey {
                name: "orders_pkey".into(),
                columns: vec!["id".into()],
                is_primary: true,
            }],
            checks: vec![],
        };
        schema_of(vec![users, orders])
    }

    /// Every value written for one column, across the statement for one table.
    fn column_values(sql: &str, table: &str, index: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut inside = false;
        for line in sql.lines() {
            if line.starts_with("INSERT INTO") {
                inside = line.contains(&format!("\"{table}\""));
                continue;
            }
            if inside && line.trim_start().starts_with('(') {
                let inner = line.trim().trim_end_matches([',', ';']);
                let inner = inner.trim_start_matches('(').trim_end_matches(')');
                if let Some(v) = inner.split(", ").nth(index) {
                    out.push(v.to_string());
                }
            }
        }
        out
    }

    #[test]
    fn a_foreign_key_only_ever_names_a_row_that_was_written() {
        // The property the whole emitter exists for. Generating a plausible
        // integer here would produce rows the database rejects.
        let s = users_and_orders();
        let out = render(&s, 10);
        let parents = column_values(&out, "users", 0);
        let children = column_values(&out, "orders", 1);

        assert_eq!(parents.len(), 10);
        assert_eq!(children.len(), 10);
        for child in &children {
            assert!(
                parents.contains(child),
                "{child} was never written to users"
            );
        }
    }

    #[test]
    fn parents_are_written_before_children() {
        let out = render(&users_and_orders(), 3);
        let users_at = out.find("\"users\"").unwrap();
        let orders_at = out.find("\"orders\"").unwrap();
        assert!(users_at < orders_at);
    }

    #[test]
    fn a_unique_column_is_unique_across_the_statement() {
        let out = render(&users_and_orders(), 100);
        let emails = column_values(&out, "users", 1);
        let distinct: std::collections::BTreeSet<_> = emails.iter().collect();
        assert_eq!(distinct.len(), emails.len(), "a unique column repeated");
    }

    #[test]
    fn the_same_seed_produces_byte_identical_sql() {
        // Reproducibility, which this tool sells and which a global random
        // stream would only give for a frozen schema.
        let s = users_and_orders();
        assert_eq!(render(&s, 20), render(&s, 20));
    }

    #[test]
    fn everything_is_wrapped_in_one_transaction() {
        // All or nothing: a partial seed is worse than none, because it looks
        // like it worked.
        let out = render(&users_and_orders(), 2);
        assert!(out.starts_with("BEGIN;"));
        assert!(out.trim_end().ends_with("COMMIT;"));
    }

    #[test]
    fn a_refused_table_produces_no_rows_at_all() {
        let mut s = users_and_orders();
        s.tables
            .get_mut(&TableId::new("public", "users"))
            .unwrap()
            .checks
            .push(crate::schema::CheckConstraint {
                name: "impossible".into(),
                definition: "CHECK ((num_nonnulls(a, b) = 1))".into(),
            });
        let out = render(&s, 5);
        assert!(!out.contains("\"users\""), "a refused table was written to");
        // And its child goes with it, rather than pointing at nothing.
        assert!(!out.contains("\"orders\""));
    }

    #[test]
    fn a_table_of_only_defaults_still_gets_rows() {
        let mut t = Table {
            id: TableId::new("public", "pings"),
            columns: vec![col("id", ColumnType::Integer { bytes: 4 }, false)],
            foreign_keys: vec![],
            unique_keys: vec![],
            checks: vec![],
        };
        t.columns[0].has_default = true;
        let out = render(&schema_of(vec![t]), 3);
        assert_eq!(out.matches("DEFAULT VALUES").count(), 3);
    }
}
