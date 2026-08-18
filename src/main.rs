//! `pgsow` — read a Postgres schema and produce data that satisfies it.
//!
//! Point it at a connection string. It reads the schema, works out the insert
//! order, and either prints the SQL or writes the rows — or names the table it
//! will not touch, and the constraint that stopped it.
//!
//! Three outcomes, never two: filled, refused, or could-not-read. The failure
//! mode of a seed tool is not that it crashes; it is that it inserts plausible
//! rows which quietly violate a rule nobody re-checked, and then everything
//! downstream is tested against data the real system would have rejected.

use std::collections::BTreeSet;
use std::io::Write;

use clap::Parser;
use pgsow::{classify, dsn, emit, filter, graph, introspect};

#[derive(Parser)]
#[command(
    name = "pgsow",
    version,
    about = "Reads a Postgres schema and says what it could fill — or names the table it will not touch, and why"
)]
struct Args {
    /// Connection string. Falls back to $DATABASE_URL.
    #[arg(long, env = "DATABASE_URL")]
    dsn: String,

    /// Schemas to read.
    #[arg(long, default_value = "public")]
    schema: Vec<String>,

    /// Rows per table: a number, or `table=number` to override one. The
    /// pattern takes `*` and `?`, and the last one that matches wins.
    #[arg(long, value_name = "N|TABLE=N")]
    rows: Vec<String>,

    /// Only these tables. Repeatable, and takes `*` and `?`.
    #[arg(long, value_name = "PATTERN")]
    include: Vec<String>,

    /// Never these tables. Repeatable, and beats --include on a conflict.
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,

    /// Deterministic seed. The same seed and schema give byte-identical SQL.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Write the SQL here instead of to stdout.
    #[arg(long, value_name = "FILE")]
    out: Option<std::path::PathBuf>,

    /// Report what would be filled, and generate nothing.
    #[arg(long)]
    plan: bool,

    /// Write the rows into the database instead of printing SQL.
    #[arg(long)]
    apply: bool,

    /// Empty the target tables first, in dependency order. Only with --apply.
    #[arg(long)]
    truncate: bool,

    /// Write even though the target tables already hold rows.
    #[arg(long)]
    allow_nonempty: bool,

    /// Write to a database that is not on this machine.
    #[arg(long)]
    remote: bool,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("pgsow: {message}");
            std::process::ExitCode::from(2)
        }
    }
}

fn run() -> Result<std::process::ExitCode, String> {
    let args = Args::parse();
    let rows = filter::RowCounts::parse(&args.rows)?;
    let selection = filter::Selection {
        include: args.include.clone(),
        exclude: args.exclude.clone(),
    };

    // Before connecting, because the cheapest refusal is the one that happens
    // before anything has been touched. Not a claim that the far end is
    // production — only that it is not this machine, which is a fact about the
    // string rather than a guess about the database.
    let writing = args.apply || args.truncate;
    if writing && !args.remote && !dsn::is_local(&args.dsn) {
        return Err(format!(
            "this would write to {}, which is not this machine.\n       \
             Pass --remote if that is what you meant.",
            dsn::host(&args.dsn).unwrap_or_else(|| "somewhere else".into())
        ));
    }

    let mut client = postgres::Client::connect(&args.dsn, postgres::NoTls)
        .map_err(|e| format!("cannot connect: {e}"))?;

    let read = introspect::read(&mut client, &args.schema)
        .map_err(|e| format!("cannot read the schema: {e}"))?;

    let order = graph::order(&read);
    let mut verdict = classify::classify(&read, &order);

    // Filtering happens after classification, so a table left out is simply
    // not written rather than being reported as refused. Those are different
    // answers and the report must not blur them.
    let dropped: BTreeSet<_> = verdict
        .fillable
        .iter()
        .filter(|id| !selection.allows(id))
        .cloned()
        .collect();
    verdict.fillable.retain(|id| !dropped.contains(id));
    verdict
        .deferred_repairs
        .retain(|(id, _)| !dropped.contains(id));

    for unmatched in rows.unmatched(&verdict.fillable) {
        // A mistyped override that is silently ignored produces a run which
        // looks like it worked and did something else.
        eprintln!("pgsow: --rows {unmatched} matched no table");
    }

    let options = emit::Options {
        seed: args.seed,
        rows: rows.clone(),
    };

    if args.apply {
        // The second guard, and the one that does not depend on the hostname:
        // an empty database is a scratch database whatever it is called.
        if !args.truncate && !args.allow_nonempty {
            let populated = filter::already_populated(&mut client, &verdict.fillable)
                .map_err(|e| format!("cannot tell whether the tables are empty: {e}"))?;
            if !populated.is_empty() {
                let names: Vec<String> =
                    populated.keys().take(5).map(|id| id.to_string()).collect();
                return Err(format!(
                    "{} of the target tables already hold rows — {}{}.\n       \
                     Pass --truncate to empty them, or --allow-nonempty to add to them.",
                    populated.len(),
                    names.join(", "),
                    if populated.len() > names.len() {
                        ", and others"
                    } else {
                        ""
                    }
                ));
            }
        }

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
            client
                .batch_execute(&format!("TRUNCATE {};", names.join(", ")))
                .map_err(|e| format!("could not empty the tables first: {e}"))?;
        }

        match emit::apply(&mut client, &read, &verdict, &options) {
            Ok(n) => eprintln!("pgsow: applied {n} statements"),
            // The transaction rolled back, so the database is as it was.
            Err(e) => return Err(format!("nothing was written — {e}")),
        }
    } else if !args.plan {
        let sql = emit::sql(&read, &verdict, &options);
        match &args.out {
            Some(path) => std::fs::write(path, &sql)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?,
            // stdout, so the report on stderr does not mix prose into it.
            None => {
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(sql.as_bytes())
                    .map_err(|e| format!("cannot write the SQL: {e}"))?;
            }
        }
    }

    report(&read, &verdict, &rows, &dropped);

    // 0 everything fillable · 1 something refused · 2 could not read, so this
    // composes in a script without anybody parsing the prose above.
    Ok(if verdict.refused.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    })
}

fn report(
    read: &pgsow::schema::Schema,
    verdict: &classify::Verdict,
    rows: &filter::RowCounts,
    dropped: &BTreeSet<pgsow::schema::TableId>,
) {
    eprintln!(
        "pgsow: {} tables, {} fillable, {} refused ({:.0}% reach)",
        verdict.total(),
        verdict.fillable.len(),
        verdict.refused.len(),
        verdict.reach() * 100.0,
    );

    if !dropped.is_empty() {
        // Named separately from the refusals. A table left out by --exclude
        // could have been filled and was not asked for; a refused one could
        // not be. Reporting them together would lose that.
        eprintln!("\n  left out by --include/--exclude: {}", dropped.len());
    }

    if !verdict.fillable.is_empty() {
        // With the row count beside each name, because it is not always the
        // number that was asked for. A unique boolean holds two rows and a
        // join table holds as many as it has pairs.
        let counts = pgsow::volume::plan(read, &verdict.fillable, rows);
        eprintln!("\n  would fill, in this order:");
        for id in &verdict.fillable {
            let asked = rows.for_table(id);
            let n = counts.get(id).copied().unwrap_or(asked);
            if n == asked {
                eprintln!("    {id:<32} {n}");
            } else {
                eprintln!("    {id:<32} {n}  (capped — no room for {asked})");
            }
        }
    }

    if !verdict.refused.is_empty() {
        eprintln!("\n  refused:");
        for (id, reasons) in &verdict.refused {
            for (n, reason) in reasons.iter().enumerate() {
                let label = if n == 0 {
                    id.to_string()
                } else {
                    String::new()
                };
                eprintln!("    {label:<24} {}", reason.explain());
            }
        }
    }
}
