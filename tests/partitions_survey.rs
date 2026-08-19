//! What the corpus's partitioned tables are actually shaped like.
//!
//! Partitioning is the largest single cause of refusal by reasoning — 90 of
//! the root refusals across the corpus — and the plan to widen it is only
//! worth making if the partitions are there to be read. A parent with no
//! partitions at all takes no rows however well its bounds are understood.
//!
//! `cargo test --test partitions_survey -- --ignored --nocapture`

mod harness;

use std::collections::BTreeMap;
use std::path::Path;

use harness::Db;

mod corpus_shared;
use corpus_shared as shared;

#[test]
#[ignore]
fn what_the_partitioned_tables_look_like() {
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    let mut empty_parents = 0usize;
    let mut readable = 0usize;

    for source in shared::sources() {
        let name = &source.name;
        let path = Path::new("tests/corpus").join(format!("{name}.sql"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let db = Db::start();
        let mut client = db.client();
        let schemas = shared::load(&mut client, &text);

        let sql = "
            SELECT n.nspname, c.relname,
                   pg_get_partkeydef(c.oid),
                   (SELECT count(*) FROM pg_inherits i WHERE i.inhparent = c.oid),
                   COALESCE((SELECT array_agg(pg_get_expr(p.relpartbound, p.oid))
                             FROM pg_inherits i
                             JOIN pg_class p ON p.oid = i.inhrelid
                             WHERE i.inhparent = c.oid), '{}')
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE c.relkind = 'p' AND n.nspname = ANY($1)";
        let Ok(rows) = client.query(sql, &[&schemas]) else {
            continue;
        };
        if rows.is_empty() {
            continue;
        }

        let mut here: BTreeMap<String, usize> = BTreeMap::new();
        for row in &rows {
            let key: String = row.get(2);
            let children: i64 = row.get(3);
            let bounds: Vec<String> = row.get(4);
            let kind = key.split_whitespace().next().unwrap_or("?").to_string();

            let label = if children == 0 {
                empty_parents += 1;
                format!("{kind} with no partitions at all")
            } else {
                match pgsow::partitions::interpret(&key, &bounds) {
                    pgsow::partitions::Routing::Anything => {
                        readable += 1;
                        format!("{kind} read as unconstrained")
                    }
                    pgsow::partitions::Routing::OneOf { .. } => {
                        readable += 1;
                        format!("{kind} read as a value set")
                    }
                    pgsow::partitions::Routing::Unknown => format!("{kind} NOT READ"),
                }
            };
            *here.entry(label.clone()).or_default() += 1;
            *totals.entry(label).or_default() += 1;
        }
        println!("  {name}: {} partitioned parents", rows.len());
        for (what, n) in &here {
            println!("      {n:>4}  {what}");
        }
    }

    println!("\nACROSS THE CORPUS");
    for (what, n) in &totals {
        println!("  {n:>4}  {what}");
    }
    println!(
        "\n{empty_parents} parents have no partitions at all and can take no row \
         whatever is understood about them.\n{readable} are read today."
    );
}
