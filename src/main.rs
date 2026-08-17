//! `pgsow` — read a Postgres schema and produce data that satisfies it.
//!
//! **First milestone: introspect and classify only.** It reads the schema,
//! works out the insert order, and decides for every table whether it could be
//! filled or must be refused. It generates nothing yet, deliberately.
//!
//! That is not a half-finished tool; it is the measurement that decides
//! whether the rest is worth writing. The number to look at is *reach* — the
//! share of real tables that can be ordered and classified without ambiguity.
//! If that is poor on real schemas, no amount of clever value generation
//! rescues it, and better to know on day one than in month three.

use clap::Parser;
use pgsow::{classify, emit, graph, introspect};

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

    /// Rows per table.
    #[arg(long, default_value_t = 50)]
    rows: usize,

    /// Deterministic seed. The same seed and schema give byte-identical SQL.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Report what would be filled, and generate nothing.
    #[arg(long)]
    plan: bool,

    /// Write the rows into the database instead of printing SQL.
    #[arg(long)]
    apply: bool,

    /// Empty the target tables first, in dependency order. Only with --apply.
    #[arg(long)]
    truncate: bool,
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

    let options = emit::Options { seed: args.seed, rows: args.rows };

    // The SQL goes to stdout so it can be piped, redirected or read. The
    // report goes to stderr so that doing so does not mix prose into it.
    if args.apply {
        if args.truncate && !verdict.fillable.is_empty() {
            // Reverse dependency order, so a parent is never emptied while a
            // child still points at it. CASCADE is deliberately not used: it
            // would silently empty tables this was never asked to touch.
            let names: Vec<String> = verdict
                .fillable
                .iter()
                .rev()
                .map(|id| id.quoted())
                .collect();
            if let Err(e) = client.batch_execute(&format!("TRUNCATE {};", names.join(", "))) {
                eprintln!("pgsow: could not empty the tables first: {e}");
                return std::process::ExitCode::from(2);
            }
        }
        match emit::apply(&mut client, &read, &verdict, &options) {
            Ok(n) => eprintln!("pgsow: applied {n} statements"),
            Err(e) => {
                // The transaction rolled back, so the database is as it was.
                eprintln!("pgsow: nothing was written — {e}");
                return std::process::ExitCode::from(2);
            }
        }
    } else if !args.plan {
        print!("{}", emit::sql(&read, &verdict, &options));
    }

    eprintln!(
        "pgsow: {} tables, {} fillable, {} refused ({:.0}% reach)",
        verdict.total(),
        verdict.fillable.len(),
        verdict.refused.len(),
        verdict.reach() * 100.0,
    );

    if !verdict.fillable.is_empty() {
        // With the row count beside each name, because it is not always the
        // number that was asked for. A unique boolean holds two rows and a
        // join table holds as many as it has pairs; getting two where fifty
        // were requested should be visible rather than discovered later.
        let counts = pgsow::volume::plan(&read, &verdict.fillable, args.rows);
        eprintln!("\n  would fill, in this order:");
        for id in &verdict.fillable {
            let n = counts.get(id).copied().unwrap_or(args.rows);
            if n == args.rows {
                eprintln!("    {id:<32} {n}");
            } else {
                eprintln!("    {id:<32} {n}  (capped — no room for {})", args.rows);
            }
        }
    }

    if !verdict.refused.is_empty() {
        eprintln!("\n  refused:");
        for (id, reasons) in &verdict.refused {
            for (n, reason) in reasons.iter().enumerate() {
                let label = if n == 0 { id.to_string() } else { String::new() };
                eprintln!("    {label:<24} {}", reason.explain());
            }
        }
    }

    // 0 everything fillable · 1 something refused · 2 could not read, so this
    // composes in a script without anybody parsing the prose above.
    if verdict.refused.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
