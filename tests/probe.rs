//! What asking the database is worth, measured across the corpus.
//!
//! Reach by reasoning alone is 63%. This runs `probe::run` — the real thing,
//! not a model of it — against every schema in the corpus and reports what the
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
    let only = std::env::var("PGSEED_ONLY").unwrap_or_default();

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
        let schemas = shared::load(&mut client, &text);

        let Ok(read) = pgseed::introspect::read(&mut client, &schemas) else {
            continue;
        };
        let order = pgseed::graph::order(&read);
        let verdict = pgseed::classify::classify(&read, &order);
        let options = pgseed::emit::Options::flat(1, 5);

        // Rolled back, so the measurement leaves nothing behind and the next
        // schema starts from the same place.
        let outcome = match pgseed::probe::run(
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

        // With one schema named, the point is diagnosis rather than a number:
        // first what reasoning refused on its own terms, since a root refusal
        // is worth twenty of the refusals it causes.
        if !only.is_empty() {
            for (id, reasons) in &verdict.refused {
                for reason in reasons {
                    let line = reason.explain();
                    if !line.contains("which is itself refused") {
                        println!(
                            "      ROOT  {id}: {}",
                            line.chars().take(150).collect::<String>()
                        );
                    }
                }
            }
        }

        // Then what the database said about the ones probing could not save:
        // what is still refused, and what the database said about it.
        if !only.is_empty() {
            let mut counted: std::collections::BTreeMap<String, usize> = Default::default();
            for rejected in &outcome.still_refused {
                *counted
                    .entry(format!(
                        "{} — {} {}",
                        rejected.table, rejected.code, rejected.message
                    ))
                    .or_default() += 1;
            }
            let mut ranked: Vec<_> = counted.into_iter().map(|(k, v)| (v, k)).collect();
            ranked.sort_by_key(|entry| std::cmp::Reverse(entry.0));
            for (n, what) in ranked.iter().take(2000) {
                println!(
                    "      {n:>3}  {}",
                    what.chars().take(110).collect::<String>()
                );
            }
        }

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

/// Which refused tables the database fills for you anyway.
///
/// A refusal is not always a loss. Zitadel's `login_names` is a *projection*:
/// triggers on `users`, `org_domains` and the domain-policy tables write it,
/// and a real Zitadel never inserts into it directly. Refusing it is therefore
/// correct rather than cautious — a row written into it by hand would describe
/// a login name derived from nothing, which is worse than no row at all.
///
/// So this counts, after a probed run, how many rows each still-refused table
/// actually holds. A table with rows in it was populated by the database, and
/// a refusal that costs nothing belongs in a different sentence from one that
/// costs a table.
///
/// `PGSEED_ONLY=zitadel cargo test --test probe -- --ignored --nocapture fills_for_you`
#[test]
#[ignore]
fn what_the_database_fills_for_you() {
    let only = std::env::var("PGSEED_ONLY").unwrap_or_default();

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
        let schemas = shared::load(&mut client, &text);
        let Ok(read) = pgseed::introspect::read(&mut client, &schemas) else {
            continue;
        };
        let order = pgseed::graph::order(&read);
        let verdict = pgseed::classify::classify(&read, &order);
        let options = pgseed::emit::Options::flat(1, 5);

        // Kept, not rolled back: the question is what is in the database
        // afterwards, which is the thing a user would look at.
        let Ok(outcome) = pgseed::probe::run(
            &mut client,
            &read,
            &verdict,
            &order,
            &options,
            true,
            &mut |_| {},
        ) else {
            continue;
        };

        let mut filled_anyway = 0usize;
        let mut still_empty = 0usize;
        for rejected in &outcome.still_refused {
            let count: i64 = client
                .query_one(
                    &format!("SELECT count(*) FROM {}", rejected.table.quoted()),
                    &[],
                )
                .map(|row| row.get(0))
                .unwrap_or(0);
            if count > 0 {
                filled_anyway += 1;
                println!(
                    "  {name}: {} holds {count} rows the database wrote",
                    rejected.table
                );
            } else {
                still_empty += 1;
            }
        }
        println!(
            "  {name}: {} refused after probing — {filled_anyway} filled by the database anyway, {still_empty} empty",
            outcome.still_refused.len()
        );
    }
}

/// `--probe` must respect `--include`.
///
/// It did not. `probe::run` builds its own optimistic verdict from
/// `order.tables` — every table in the schema — while `--include` filters
/// `verdict.fillable` only. So asking for one table and passing `--probe`
/// offered the whole database to the server.
///
/// Found from the outside: pgplan's corpus survey could not measure GitLab or
/// Sourcegraph inside a seven-minute budget while asking for six tables each.
#[test]
fn probe_does_not_fill_tables_that_include_left_out() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE wanted (id serial PRIMARY KEY, name text NOT NULL);
         CREATE TABLE unwanted (id serial PRIMARY KEY, name text NOT NULL);",
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pgseed"))
        .args([
            "--dsn",
            &db.url,
            "--apply",
            "--rows",
            "5",
            "--allow-nonempty",
            "--probe",
            "--include",
            "wanted",
        ])
        .output()
        .expect("pgseed should run");
    assert!(
        out.status.success(),
        "pgseed failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut client = db.client();
    let wanted: i64 = client
        .query_one("SELECT count(*) FROM wanted", &[])
        .unwrap()
        .get(0);
    let unwanted: i64 = client
        .query_one("SELECT count(*) FROM unwanted", &[])
        .unwrap()
        .get(0);

    assert_eq!(wanted, 5, "the included table should have been filled");
    assert_eq!(
        unwanted, 0,
        "--include named one table and --probe filled {unwanted} rows into another"
    );
}
