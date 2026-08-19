//! What the corpus's triggers actually do.
//!
//! A survey, not an assertion. The plan offered two ways to handle a trigger —
//! refuse every table carrying one, or read a closed set of bodies, and the
//! choice between them is a question about real triggers rather than a matter
//! of taste. This counts them.
//!
//! `cargo test --test triggers -- --ignored --nocapture`

mod harness;

use std::path::Path;

use harness::Db;

mod corpus_shared;
use corpus_shared as shared;

#[test]
#[ignore]
fn what_the_triggers_do() {
    let (mut tables_with, mut raising, mut total) = (0usize, 0usize, 0usize);
    let mut bodies: Vec<(String, String, String)> = Vec::new();

    for source in shared::sources() {
        let name = &source.name;
        let path = Path::new("tests/corpus").join(format!("{name}.sql"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let db = Db::start();
        let mut client = db.client();
        let schemas = shared::load(&mut client, &text);

        // Row-level triggers that fire on insert are the only ones that can
        // refuse a row this writes. A statement-level trigger, or one that
        // fires on update or delete, cannot.
        let sql = "
            SELECT c.relname, t.tgname, p.prosrc
            FROM pg_trigger t
            JOIN pg_class c ON c.oid = t.tgrelid
            JOIN pg_proc p ON p.oid = t.tgfoid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE NOT t.tgisinternal
              AND (t.tgtype & 1) <> 0      -- row level
              AND (t.tgtype & 4) <> 0      -- fires on INSERT
              AND n.nspname = ANY($1)";
        let Ok(rows) = client.query(sql, &[&schemas]) else {
            continue;
        };

        let mut seen: std::collections::BTreeSet<String> = Default::default();
        for row in &rows {
            total += 1;
            let table: String = row.get(0);
            let trigger: String = row.get(1);
            let body: String = row.get(2);
            seen.insert(table.clone());
            // The only thing that makes a trigger able to refuse a row.
            let upper = body.to_uppercase();
            if upper.contains("RAISE") || upper.contains("ASSERT") {
                raising += 1;
                if bodies.len() < 12 {
                    bodies.push((name.to_string(), format!("{table}.{trigger}"), body));
                }
            }
        }
        tables_with += seen.len();
        if !rows.is_empty() {
            println!(
                "  {name}: {} row-level insert triggers on {} tables",
                rows.len(),
                seen.len()
            );
        }
    }

    println!(
        "\nTOTALS\t{total} row-level insert triggers on {tables_with} tables, \
         {raising} of them can raise"
    );
    for (schema, what, body) in &bodies {
        let first: String = body
            .lines()
            .filter(|l| l.to_uppercase().contains("RAISE") || l.to_uppercase().contains("ASSERT"))
            .take(1)
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  RAISES\t{schema}\t{what}\t{}",
            first.trim().chars().take(90).collect::<String>()
        );
    }
}
