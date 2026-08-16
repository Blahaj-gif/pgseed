//! `pgsow` — read a Postgres schema and produce data that satisfies it.
//!
//! **First milestone: introspect and classify only.** It reads the schema,
//! works out the insert order, and decides for every table whether it could be
//! filled or must be refused. It generates nothing yet, deliberately.
//!
//! That is not a half-finished version of the tool; it is the measurement that
//! decides whether the rest is worth writing. The number to look at is
//! `reach` — the share of real tables that can be ordered and classified
//! without ambiguity. If that is poor on real schemas, no amount of clever
//! value generation rescues it, and better to know on day one.

// The schema model records everything introspection can see, and this
// milestone only *classifies*. Primary keys, referenced columns and column
// positions are read and not yet consumed — they are what the generation
// milestone needs, and dropping them now would mean querying for them again
// later. Warned about rather than deleted, and this allow comes off with the
// first generator.
#![allow(dead_code)]

mod classify;
mod graph;
mod introspect;
mod schema;

use clap::Parser;

#[derive(Parser)]
#[command(name = "pgsow", version, about =
    "Reads a Postgres schema and says what it could fill — or names the table it will not touch, and why")]
struct Args {
    /// Connection string. Falls back to $DATABASE_URL.
    #[arg(long, env = "DATABASE_URL")]
    dsn: String,

    /// Schemas to read.
    #[arg(long, default_value = "public")]
    schema: Vec<String>,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();

    let mut client = match postgres::Client::connect(&args.dsn, postgres::NoTls) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pgsow: cannot connect: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    let read = match introspect::read(&mut client, &args.schema) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pgsow: cannot read the schema: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    let order = graph::order(&read);
    let verdict = classify::classify(&read, &order);

    println!(
        "pgsow: {} tables, {} fillable, {} refused ({:.0}% reach)",
        verdict.total(),
        verdict.fillable.len(),
        verdict.refused.len(),
        verdict.reach() * 100.0,
    );

    if !verdict.fillable.is_empty() {
        println!("\n  would fill, in this order:");
        for id in &verdict.fillable {
            println!("    {id}");
        }
    }

    if !verdict.refused.is_empty() {
        println!("\n  refused:");
        for (id, reasons) in &verdict.refused {
            for (n, reason) in reasons.iter().enumerate() {
                let label = if n == 0 { id.to_string() } else { String::new() };
                println!("    {label:<24} {}", reason.explain());
            }
        }
    }

    // 0 everything fillable · 1 something refused · 2 could not read.
    // So it composes in a script without anybody parsing this prose.
    if verdict.refused.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
