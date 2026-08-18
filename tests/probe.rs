//! What asking the database is worth, measured across the corpus.
//!
//! Reach by reasoning alone is 63%. This runs `probe::run` — the real thing,
//! not a model of it — against all twenty schemas and reports what the
//! database accepts on top of that. The number it produces is the one quoted
//! in the README, so it is measured by the code that ships rather than by a
//! survey that resembles it.
//!
//! `cargo test --test probe -- --ignored --nocapture`

mod harness;

use std::path::Path;

use harness::Db;

mod corpus_shared;
use corpus_shared as shared;

#[test]
#[ignore]
fn how_far_the_database_gets_us() {
    let (mut all_tables, mut all_fillable, mut all_rescued) = (0usize, 0usize, 0usize);

    // One schema at a time when something needs looking at: this takes four
    // minutes over the whole corpus and a diagnosis rarely needs all of it.
    let only = std::env::var("PGSOW_ONLY").unwrap_or_default();

    for source in shared::sources() {
        let name = &source.name;
        if !only.is_empty() && !only.split(',').any(|want| want == name) {
            continue;
        }
        let path = Path::new("tests/corpus").join(format!("{name}.sql"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };

        let db = Db::start();
        let mut client = db.client();
        for schema_name in shared::schemas_in(&text) {
            let _ =
                client.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema_name}\";"));
        }
        for statement in shared::statements(&text) {
            let _ = client.batch_execute(&statement);
        }

        let schemas = shared::schemas_in(&text);
        let Ok(read) = pgsow::introspect::read(&mut client, &schemas) else {
            continue;
        };
        let order = pgsow::graph::order(&read);
        let verdict = pgsow::classify::classify(&read, &order);
        let options = pgsow::emit::Options::flat(1, 5);

        // Rolled back, so the measurement leaves nothing behind and the next
        // schema starts from the same place.
        let outcome = match pgsow::probe::run(
            &mut client,
            &read,
            &verdict,
            &order,
            &options,
            false,
            &mut |_| {},
        ) {
            Ok(outcome) => outcome,
            Err(reason) => {
                // A table this claimed to understand and could not fill. That
                // is the corpus gate's failure, reported here rather than
                // swallowed.
                println!("  {name}: BROKEN PROMISE — {reason}");
                continue;
            }
        };

        let total = verdict.total();
        all_tables += total;
        all_fillable += verdict.fillable.len();
        all_rescued += outcome.rescued.len();
        println!(
            "  {name}: {} of {total} by reasoning ({:.0}%) → {} more from the database, {:.0}%",
            verdict.fillable.len(),
            100.0 * verdict.fillable.len() as f64 / total.max(1) as f64,
            outcome.rescued.len(),
            100.0 * outcome.reach(&verdict),
        );
    }

    println!(
        "\nTOTALS\t{all_fillable} of {all_tables} by reasoning ({:.1}%). \
         The database accepts {all_rescued} more, which is {:.1}%.",
        100.0 * all_fillable as f64 / all_tables.max(1) as f64,
        100.0 * (all_fillable + all_rescued) as f64 / all_tables.max(1) as f64,
    );
}
