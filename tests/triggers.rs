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
    // The measurement the plan asks for before anybody narrows the rule: how
    // many interfering triggers touch nothing that was being relied on.
    let (mut interfering, mut harmless_assignment) = (0usize, 0usize);
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

        // The schema as this tool reads it, so the question is asked of the
        // real columns and the real keys rather than of the trigger text.
        let schema_tables: std::collections::BTreeMap<String, pgsow::schema::Table> =
            match pgsow::introspect::read(&mut client, &schemas) {
                Ok(read) => read
                    .tables
                    .values()
                    .map(|t| (t.id.name.clone(), t.clone()))
                    .collect(),
                Err(_) => Default::default(),
            };

        let mut seen: std::collections::BTreeSet<String> = Default::default();
        for row in &rows {
            total += 1;
            let table: String = row.get(0);
            let trigger: String = row.get(1);
            let body: String = row.get(2);
            seen.insert(table.clone());

            // What a narrower rule would have to decide, per trigger.
            if let Some(target) = schema_tables.get(&table) {
                let names: Vec<String> = target
                    .columns_to_write()
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
                if pgsow::triggers::interferes(&body, &names) {
                    interfering += 1;
                    let assigned = pgsow::triggers::assigns_to_new(&body);
                    let raises = {
                        let text = body.to_uppercase();
                        text.contains("RAISE") || text.contains("ASSERT")
                    };
                    // An assignment, and only to columns nothing depends on:
                    // nullable, in no unique key, named by no CHECK.
                    let nothing_relied_on = !raises
                        && !assigned.is_empty()
                        && assigned.iter().all(|column| {
                            target.column(column).is_some_and(|c| c.nullable)
                                && !target
                                    .unique_keys
                                    .iter()
                                    .any(|k| k.columns.iter().any(|c| c == column))
                                && !target
                                    .checks
                                    .iter()
                                    .any(|c| c.definition.contains(column.as_str()))
                        });
                    if nothing_relied_on {
                        harmless_assignment += 1;
                    }
                }
            }

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
    println!(
        "
{harmless_assignment} of the {interfering} that interfere assign only to columns 
         that are nullable, in no unique key, and named by no CHECK. Those are the 
         ones a narrower rule would let through: an assignment only matters if 
         something was relying on the value, and NULL into an unconstrained 
         nullable column is not a rejected row."
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
