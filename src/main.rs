//! `pgseed` — read a Postgres schema and produce data that satisfies it.
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
use pgseed::{classify, dsn, emit, filter, graph, introspect};

/// What the database actually said.
///
/// `postgres::Error` renders as `db error` and nothing else, which is the
/// least useful sentence available at the moment it matters most: the first
/// thing a new user gets wrong is the connection string, and `cannot connect:
/// db error` tells them nothing about which part.
use pgseed::dberror::explain;

#[derive(Parser)]
#[command(
    name = "pgseed",
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

    /// Offer the refused tables to the database and keep the rows it accepts.
    ///
    /// Writes — a probe is a real INSERT inside a savepoint — so it is guarded
    /// the same way --apply is, and without --apply the whole transaction is
    /// rolled back and only the accepted SQL is printed.
    #[arg(long)]
    probe: bool,

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
            eprintln!("pgseed: {message}");
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
    let writing = args.apply || args.truncate || args.probe;
    if writing && !args.remote && !dsn::is_local(&args.dsn) {
        return Err(format!(
            "this would write to {}, which is not this machine.\n       \
             Pass --remote if that is what you meant.",
            dsn::host(&args.dsn).unwrap_or_else(|| "somewhere else".into())
        ));
    }

    let mut client = postgres::Client::connect(&args.dsn, postgres::NoTls)
        .map_err(|e| format!("cannot connect: {}", explain(&e)))?;

    let read = introspect::read(&mut client, &args.schema)
        .map_err(|e| format!("cannot read the schema: {}", explain(&e)))?;

    // A mistyped `--schema` used to produce "0 tables, 0 fillable" and exit 0,
    // which looks exactly like a schema that is genuinely empty. Silence is
    // the one answer this tool is not allowed to give.
    if let Ok(rows) = client.query(
        "SELECT n FROM unnest($1::text[]) AS n          WHERE n NOT IN (SELECT nspname FROM pg_namespace)",
        &[&args.schema],
    ) {
        for row in &rows {
            let missing: String = row.get(0);
            eprintln!("pgseed: there is no schema called {missing}");
        }
    }

    // And the schema that exists and is empty, while the tables are somewhere
    // else. Hasura keeps its catalog in `hdb_catalog` and Zitadel in
    // `zitadel`, so pointing this at either with the default `--schema public`
    // printed "0 tables, 0 fillable" and exited 0. That reads as "your
    // database is empty", which is a different and wrong answer.
    if read.is_empty() {
        if let Ok(rows) = client.query(
            "SELECT ns.nspname, count(*) FROM pg_class c              JOIN pg_namespace ns ON ns.oid = c.relnamespace              WHERE c.relkind IN ('r', 'p')                AND ns.nspname NOT IN ('pg_catalog', 'information_schema')                AND ns.nspname <> ALL($1::text[])              GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 5",
            &[&args.schema],
        ) {
            let elsewhere: Vec<String> = rows
                .iter()
                .map(|row| {
                    let name: String = row.get(0);
                    let held: i64 = row.get(1);
                    format!("{name} ({held})")
                })
                .collect();
            if !elsewhere.is_empty() {
                eprintln!(
                    "pgseed: no tables in {}, but there are tables in {} — pass --schema to read one of those",
                    args.schema.join(", "),
                    elsewhere.join(", ")
                );
            }
        }
    }

    let order = graph::order(&read);
    let mut verdict = classify::classify(&read, &order);
    // Filled in by --probe, and printed apart from the tables that were
    // understood: a row the database accepted and a row this could show was
    // right are different kinds of confidence, and one number for both throws
    // away the distinction the tool exists for.
    let mut probed: Option<pgseed::probe::Outcome> = None;

    // Filtering happens after classification, so a table left out is simply
    // not written rather than being reported as refused. Those are different
    // answers and the report must not blur them.
    let mut dropped: BTreeSet<_> = verdict
        .fillable
        .iter()
        .chain(verdict.refused.iter().map(|(id, _)| id))
        .filter(|id| !selection.allows(id))
        .cloned()
        .collect();
    verdict.fillable.retain(|id| !dropped.contains(id));
    // Refused tables are filtered too. They were not, and a run asking only
    // for `--include 'Kunden*'` reported "1 refused" and named a table the
    // user had just excluded — along with a reach figure computed over it.
    // A table nobody asked about is not a refusal.
    verdict.refused.retain(|(id, _)| !dropped.contains(id));
    verdict
        .deferred_repairs
        .retain(|(id, _)| !dropped.contains(id));
    dropped.retain(|id| read.tables.contains_key(id));

    for unmatched in rows.unmatched(&verdict.fillable) {
        // A mistyped override that is silently ignored produces a run which
        // looks like it worked and did something else.
        eprintln!("pgseed: --rows {unmatched} matched no table");
    }

    let options = emit::Options {
        seed: args.seed,
        rows: rows.clone(),
    };

    if args.apply {
        // The second guard, and the one that does not depend on the hostname:
        // an empty database is a scratch database whatever it is called.
        if !args.truncate && !args.allow_nonempty {
            let populated =
                filter::already_populated(&mut client, &verdict.fillable).map_err(|e| {
                    format!("cannot tell whether the tables are empty: {}", explain(&e))
                })?;
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
            // Postgres will not empty a table that something else references,
            // even when that something else is empty. A refused table with a
            // foreign key into a filled one is an ordinary shape — an
            // `invoices` this could not generate, pointing at an `orders` it
            // could — and it used to make `--truncate` fail outright.
            //
            // So the targets, plus everything that points at them, however far
            // that reaches. CASCADE would do the same thing; the objection to
            // CASCADE was that it does it *silently*, and the extra tables are
            // named below. A row in `invoices` whose `orders` row has just
            // been deleted was not going to survive this either way.
            let also = dependents_of(&read, &verdict.fillable);
            let names: Vec<String> = verdict
                .fillable
                .iter()
                .rev()
                .chain(also.iter())
                .map(|id| id.quoted())
                .collect();
            if !also.is_empty() {
                eprintln!(
                    "pgseed: also emptying {} at the targets — {}",
                    if also.len() == 1 {
                        "1 table that points".to_string()
                    } else {
                        format!("{} tables that point", also.len())
                    },
                    also.iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            client
                .batch_execute(&format!("TRUNCATE {};", names.join(", ")))
                .map_err(|e| format!("could not empty the tables first: {}", explain(&e)))?;
        }

        if args.probe {
            let outcome = pgseed::probe::run(
                &mut client,
                &read,
                &verdict,
                &order,
                &options,
                true,
                &mut |_| {},
            )
            .map_err(|e| format!("nothing was written — {e}"))?;
            eprintln!("pgseed: applied {} statements", outcome.kept);
            probed = Some(outcome);
        } else {
            match emit::apply(&mut client, &read, &verdict, &options) {
                Ok(n) => eprintln!("pgseed: applied {n} statements"),
                // The transaction rolled back, so the database is as it was.
                Err(e) => return Err(format!("nothing was written — {}", explain(&e))),
            }
        }
    } else if args.plan {
        // Nothing is written and nothing is kept, but the question "what would
        // I actually get" is worth an honest answer, and for --probe that
        // answer is only knowable by asking. The transaction is rolled back.
        if args.probe {
            probed = Some(pgseed::probe::run(
                &mut client,
                &read,
                &verdict,
                &order,
                &options,
                false,
                &mut |_| {},
            )?);
        }
    } else {
        // Streamed rather than built and then written: a large schema at a
        // large row count is hundreds of megabytes, and there is no reason to
        // hold it when each statement is finished before the next begins.
        let mut target: Box<dyn std::io::Write> = match &args.out {
            Some(path) => Box::new(std::io::BufWriter::new(
                std::fs::File::create(path)
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?,
            )),
            // stdout, so the report on stderr does not mix prose into it.
            None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
        };
        if args.probe {
            // The rows go in, the database rules on each, and the transaction
            // is rolled back — so the only thing that survives is the SQL for
            // the statements it kept.
            use std::io::Write as _;
            let mut failure = None;
            let outcome = pgseed::probe::run(
                &mut client,
                &read,
                &verdict,
                &order,
                &options,
                false,
                &mut |statement| {
                    if failure.is_none() {
                        if let Err(e) = writeln!(target, "\n{statement}") {
                            failure = Some(e);
                        }
                    }
                },
            )?;
            if let Some(e) = failure {
                return Err(format!("cannot write the SQL: {e}"));
            }
            target
                .flush()
                .map_err(|e| format!("cannot write the SQL: {e}"))?;
            probed = Some(outcome);
        } else {
            emit::write_sql(&mut target, &read, &verdict, &options)
                .and_then(|()| target.flush())
                .map_err(|e| format!("cannot write the SQL: {e}"))?;
        }
    }

    report(
        &read,
        &verdict,
        &rows,
        &dropped,
        probed.as_ref(),
        args.apply,
    );

    // 0 everything fillable · 1 something refused · 2 could not read, so this
    // composes in a script without anybody parsing the prose above.
    Ok(if verdict.refused.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    })
}

/// Every table that points at one of `targets`, directly or through another,
/// and is not already among them.
///
/// Emptying a table requires emptying whatever references it, so this is the
/// closure rather than one step of it: a `payments` that points at `invoices`
/// that points at `orders` blocks the truncate just as surely.
fn dependents_of(
    schema: &pgseed::schema::Schema,
    targets: &[pgseed::schema::TableId],
) -> Vec<pgseed::schema::TableId> {
    let mut wanted: BTreeSet<_> = targets.iter().cloned().collect();
    let mut added = true;
    while added {
        added = false;
        for table in schema.tables.values() {
            if wanted.contains(&table.id) {
                continue;
            }
            if table
                .foreign_keys
                .iter()
                .any(|fk| wanted.contains(&fk.references))
            {
                wanted.insert(table.id.clone());
                added = true;
            }
        }
    }
    let targets: BTreeSet<_> = targets.iter().cloned().collect();
    wanted.difference(&targets).cloned().collect()
}

fn report(
    read: &pgseed::schema::Schema,
    verdict: &classify::Verdict,
    rows: &filter::RowCounts,
    dropped: &BTreeSet<pgseed::schema::TableId>,
    probed: Option<&pgseed::probe::Outcome>,
    written: bool,
) {
    eprintln!(
        "pgseed: {} {}, {} fillable, {} refused ({:.0}% reach)",
        verdict.total(),
        if verdict.total() == 1 {
            "table"
        } else {
            "tables"
        },
        verdict.fillable.len(),
        verdict.refused.len(),
        verdict.reach() * 100.0,
    );

    if let Some(outcome) = probed {
        eprintln!(
            "  of the refused, the database accepted {} and refused {} \
             ({:.0}% reach with it asked)",
            outcome.rescued.len(),
            outcome.still_refused.len(),
            outcome.reach(verdict) * 100.0,
        );
    }

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
        let counts = pgseed::volume::plan(read, &verdict.fillable, rows);
        // Tense matters more than it looks: after `--apply` the rows are in
        // the database, and a report still saying "would fill" reads like
        // nothing happened.
        eprintln!(
            "
  {}, in this order:",
            if written { "filled" } else { "would fill" }
        );
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
