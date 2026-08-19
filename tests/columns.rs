//! What the corpus's text columns are actually called.
//!
//! `generate` used to fill every string column from one list of sixteen NATO
//! words, on the grounds that this produces *valid* data rather than realistic
//! data. That reasoning holds for correctness and fails for use: a seed tool
//! whose `email` column says `bravo` is not one anybody puts in front of a
//! screenshot.
//!
//! Naming columns is guesswork unless the names come from somewhere, so they
//! come from here. This counts every text-typed column across the corpus and
//! ranks the names, so the closed set in `nouns` is a list of what real
//! schemas call things rather than a list of what I remembered.
//!
//! `cargo test --test columns -- --ignored --nocapture`

mod harness;

use std::collections::BTreeMap;
use std::path::Path;

use harness::Db;

mod corpus_shared;
use corpus_shared as shared;

#[test]
#[ignore]
fn what_the_text_columns_are_called() {
    let mut whole: BTreeMap<String, usize> = BTreeMap::new();
    let mut last: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;
    // How many of them the closed set in `nouns` actually names, and which
    // names it misses most. The second number is what says where to widen it
    // next, and it is the one that must not be guessed at.
    let mut named = 0usize;
    let mut missed: BTreeMap<String, usize> = BTreeMap::new();

    for source in shared::sources() {
        let name = &source.name;
        let path = Path::new("tests/corpus").join(format!("{name}.sql"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let db = Db::start();
        let mut client = db.client();
        let schemas = shared::load(&mut client, &text);

        // Text-typed, in a real or partitioned table, not a system column.
        let sql = "
            SELECT a.attname
            FROM pg_attribute a
            JOIN pg_class c ON c.oid = a.attrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_type t ON t.oid = a.atttypid
            WHERE c.relkind IN ('r','p')
              AND a.attnum > 0
              AND NOT a.attisdropped
              AND t.typname IN ('text','varchar','bpchar','citext')
              AND n.nspname = ANY($1)";
        let Ok(rows) = client.query(sql, &[&schemas]) else {
            continue;
        };
        for row in &rows {
            let column: String = row.get(0);
            let column = column.to_lowercase();
            total += 1;
            *whole.entry(column.clone()).or_default() += 1;
            if pgsow::nouns::of(&column).is_some() {
                named += 1;
            } else {
                *missed.entry(column.clone()).or_default() += 1;
            }
            let tail = column.rsplit('_').next().unwrap_or(&column).to_string();
            *last.entry(tail).or_default() += 1;
        }
    }

    let mut ranked: Vec<_> = whole.iter().map(|(k, v)| (*v, k.clone())).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    println!("\n{total} text columns in the corpus. Top 70 whole names:");
    for (n, name) in ranked.iter().take(70) {
        println!("  {n:>5}  {name}");
    }

    println!(
        "\n{named} of {total} text columns land on a noun ({:.0}%). \
         The other {} are filled with an ordinary word and no claim.",
        100.0 * named as f64 / total.max(1) as f64,
        total - named
    );
    let mut unnamed: Vec<_> = missed.iter().map(|(k, v)| (*v, k.clone())).collect();
    unnamed.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    println!("\nThe biggest misses, which is where to widen it next:");
    for (n, name) in unnamed.iter().take(25) {
        println!("  {n:>5}  {name}");
    }

    let mut tails: Vec<_> = last.iter().map(|(k, v)| (*v, k.clone())).collect();
    tails.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    println!("\nTop 70 final segments (what `a_b_c` is really about):");
    for (n, name) in tails.iter().take(70) {
        println!("  {n:>5}  {name}");
    }
}
