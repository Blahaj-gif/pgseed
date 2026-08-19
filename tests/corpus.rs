//! Reach, measured against schemas this project did not write.
//!
//! This is the milestone. The question that decides whether generating values
//! is worth writing at all is: **what share of real tables can be ordered and
//! classified without ambiguity?** It is answerable before a single value is
//! invented, so it is answered first.
//!
//! The schemas are `structure.sql` files taken from open-source projects —
//! GitLab and Synapse — because a corpus written by this project's own author
//! would only contain the constructs its author remembered to handle. That
//! mistake has been made before and it measures agreement rather than
//! accuracy.
//!
//! Statements are applied one at a time and failures are tolerated. A
//! production dump carries extension installs, role grants, partition
//! attachments and settings that a bare database will refuse, and the point is
//! not to replay the dump faithfully — it is to get a large, real, tangled
//! schema into a database and then ask what can be done with it.
//!
//! # What this gate does not see, and therefore does not promise
//!
//! Only table, constraint and index DDL is applied. Two kinds of rule are
//! filtered out with the noise, and both can reject a row:
//!
//!   - **Triggers.** Discourse has one that raises `require_reply_approval in
//!     category_settings is readonly` on insert. Nothing here reads triggers,
//!     so a table carrying one is filled without regard to it.
//!   - **Partition routing.** A row written to a partitioned table needs a
//!     partition whose bounds cover it, and GitLab reports `no partition of
//!     relation ... found for row`. Partitioned parents are not read at all,
//!     which is why their children are refused for pointing at something
//!     unread, but the parents themselves are simply absent.
//!
//! Both were found by the `volume` benchmark below, which applies the *whole*
//! dump rather than the filtered part, and both are named here rather than
//! left implicit. A gate measuring a database less constrained than the real
//! one has flattered this project twice already, and saying where it still
//! does is the only honest way to quote its number.

mod corpus_shared;
mod harness;

use corpus_shared::{schemas_in, statements};

use std::path::Path;

use harness::Db;

/// Which constructs a schema actually exercises.
///
/// The corpus was a list, and a list says only how many schemas there are. A
/// construct with one schema behind it is one schema away from being untested,
/// and one with none is being tested only by DDL this project wrote — which is
/// the mistake the corpus exists to avoid. Counting them makes thin coverage
/// visible, so schemas get added to fill holes rather than to reach a round
/// number.
#[derive(Debug, Default, Clone, Copy)]
struct Coverage {
    composite_key: bool,
    foreign_key_cycle: bool,
    self_reference: bool,
    deferrable_key: bool,
    partitioned: bool,
    domain_type: bool,
    enum_type: bool,
    array_column: bool,
    json_column: bool,
    generated_column: bool,
    unique_index: bool,
    trigger: bool,
    check_constraint: bool,
}

impl Coverage {
    const NAMES: [&'static str; 13] = [
        "composite key",
        "foreign key cycle",
        "self reference",
        "deferrable key",
        "partitioned table",
        "domain type",
        "enum type",
        "array column",
        "json column",
        "generated column",
        "unique index",
        "trigger",
        "check constraint",
    ];

    fn flags(&self) -> [bool; 13] {
        [
            self.composite_key,
            self.foreign_key_cycle,
            self.self_reference,
            self.deferrable_key,
            self.partitioned,
            self.domain_type,
            self.enum_type,
            self.array_column,
            self.json_column,
            self.generated_column,
            self.unique_index,
            self.trigger,
            self.check_constraint,
        ]
    }

    fn of(schema: &pgsow::schema::Schema, order: &pgsow::graph::Order) -> Coverage {
        use pgsow::schema::ColumnType;
        let mut out = Coverage {
            foreign_key_cycle: order.cycles.iter().any(|c| c.tables.len() > 1),
            ..Default::default()
        };
        for table in schema.tables.values() {
            out.composite_key |= table.unique_keys.iter().any(|k| k.columns.len() > 1);
            out.self_reference |= table
                .foreign_keys
                .iter()
                .any(|fk| fk.references == table.id);
            out.deferrable_key |= table.foreign_keys.iter().any(|fk| fk.deferrable);
            for check in &table.checks {
                out.check_constraint = true;
                out.partitioned |= check.definition.starts_with("PARTITION BY");
                out.trigger |= check.definition.starts_with("TRIGGER");
            }
            out.unique_index |= table
                .unique_keys
                .iter()
                .any(|k| k.name.starts_with("index_") || k.name.starts_with("idx"));
            for column in &table.columns {
                out.generated_column |= column.is_generated;
                match &column.type_ {
                    ColumnType::Domain { .. } => out.domain_type = true,
                    ColumnType::Enum { .. } => out.enum_type = true,
                    ColumnType::Array { .. } => out.array_column = true,
                    ColumnType::Json { .. } => out.json_column = true,
                    _ => {}
                }
            }
        }
        out
    }
}

fn measure(name: &str, path: &Path, max_lost: usize) -> Coverage {
    let Ok(sql) = std::fs::read_to_string(path) else {
        eprintln!("  {name}: not fetched, skipping");
        return Coverage::default();
    };

    let db = Db::start();
    let mut client = db.client();

    // A dump may put its tables in a schema it assumes already exists —
    // Hasura's lives in `hdb_catalog` and creates it elsewhere. Without this
    // every statement fails and the schema scores zero for a reason that has
    // nothing to do with the tool.
    let schemas = schemas_in(&sql);
    for name in &schemas {
        let _ = client.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{name}\";"));
    }
    let schemas: Vec<String> = schemas.into_iter().collect();
    let (mut applied, mut skipped) = (0usize, 0usize);
    let mut lost_constraints = 0usize;

    for statement in statements(&sql) {
        // Only the DDL that makes tables and constraints. Everything else in a
        // production dump is noise for this purpose.
        if !corpus_shared::shapes_the_schema(&statement) {
            continue;
        }
        match client.batch_execute(&statement) {
            Ok(()) => applied += 1,
            Err(e) => {
                skipped += 1;
                if skipped <= 4 {
                    let why = e
                        .as_db_error()
                        .map_or_else(|| e.to_string(), |d| d.message().into());
                    let what: String = statement.trim_start().chars().take(70).collect();
                    eprintln!(
                        "      SKIPPED {}: {what}",
                        why.chars().take(70).collect::<String>()
                    );
                }
                // A skipped ALTER TABLE ADD CONSTRAINT means the schema in the
                // database is *less* constrained than the real one, which would
                // make reach look better than it is. Counted separately,
                // because a measurement that flatters itself is worse than none.
                // Two very different failures, and only one of them matters.
                //
                // A failed CREATE TABLE means the table is simply absent, so it
                // never enters the denominator and reach stays honest.
                //
                // A failed ALTER TABLE ADD CONSTRAINT means the table IS here
                // and is *less constrained than it really is* — which makes it
                // look fillable when the real one might not be. That is the
                // number that would flatter this measurement, so it is the one
                // reported.
                let head = statement.trim_start().to_uppercase();
                // A failed unique index is exactly the same loss as a failed
                // unique constraint, and until indexes were applied at all
                // this could not have counted one. Leaving it out now would be
                // the same hole one level down.
                let lost = (head.starts_with("ALTER TABLE") && head.contains("CONSTRAINT"))
                    || head.starts_with("CREATE UNIQUE INDEX");
                if lost {
                    lost_constraints += 1;
                    if lost_constraints <= 3 {
                        let why = e
                            .as_db_error()
                            .map_or_else(|| e.to_string(), |d| d.message().into());
                        let why: String = why.chars().take(90).collect();
                        let what: String = statement.trim_start().chars().take(60).collect();
                        eprintln!(
                            "      under-constrained: {why}
        {what}"
                        );
                    }
                }
            }
        }
    }

    // `pg_dump` blanks the search path for the whole session and four of these
    // files are `pg_dump` output. Left blank, any trigger body naming a table
    // without qualifying it fails with `relation ... does not exist`, which is
    // a fact about this harness and not about the schema. Put back to what a
    // real connection has.
    let path: Vec<String> = schemas
        .iter()
        .map(|name| format!("\"{name}\""))
        .chain(["public".to_string()])
        .collect();
    let _ = client.batch_execute(&format!("SET search_path TO {};", path.join(", ")));

    // A ceiling rather than a printout. Every loss here has been read and has
    // a cause: five are constraints on tables that failed to create, so they
    // never enter the denominator and cost nothing, and GitLab's two are
    // foreign keys the replay cannot build against a partitioned parent —
    // those are real, and they make two child tables look less constrained
    // than they are. Pinned so that a change which starts dropping more fails
    // rather than quietly scoring better for it.
    assert!(
        lost_constraints <= max_lost,
        "{name}: {lost_constraints} constraints lost, ceiling is {max_lost}. 
         A schema in the database less constrained than the real one makes 
         every number below it flattering rather than wrong, which is worse."
    );

    let schema = pgsow::introspect::read(&mut client, &schemas)
        .expect("introspection failed on a real schema");
    let order = pgsow::graph::order(&schema);
    let verdict = pgsow::classify::classify(&schema, &order);

    // Why each refusal happened, which is more useful than the total.
    let (mut checks, mut types, mut cycles, mut inherited, mut unread, mut keys) =
        (0, 0, 0, 0, 0, 0);
    for (_, reasons) in &verdict.refused {
        for reason in reasons {
            match reason {
                pgsow::classify::Refusal::CheckConstraint { .. } => checks += 1,
                pgsow::classify::Refusal::UnsupportedType { .. } => types += 1,
                pgsow::classify::Refusal::UnbreakableCycle { .. } => cycles += 1,
                pgsow::classify::Refusal::DependsOnRefused { .. } => inherited += 1,
                pgsow::classify::Refusal::DependsOnUnread { .. } => unread += 1,
                pgsow::classify::Refusal::UnsatisfiableKeys { .. } => keys += 1,
            }
        }
    }

    println!(
        "\n  {name}\n    {applied} applied, {skipped} skipped, {lost_constraints} of them constraints a live table has now lost\n    \
         {} tables · {} fillable · {} refused · REACH {:.0}%\n    \
         refused because: {checks} CHECK · {types} unsupported type · \
         {cycles} unbreakable cycle · {inherited} depends on a refused table ·          {unread} depends on a table never read · {keys} unsatisfiable keys",
        schema.len(),
        verdict.fillable.len(),
        verdict.refused.len(),
        verdict.reach() * 100.0,
    );

    // The insert order must be a real topological order: every table appears
    // after everything it requires. This is the property the whole plan rests
    // on, checked against a schema nobody here designed.
    let mut seen = std::collections::BTreeSet::new();
    for id in &verdict.fillable {
        let table = schema.get(id).unwrap();
        for fk in &table.foreign_keys {
            if fk.references == *id || !schema.tables.contains_key(&fk.references) {
                continue;
            }
            if verdict.is_refused(&fk.references) || fk.is_optional(table) {
                continue;
            }
            assert!(
                seen.contains(&fk.references),
                "{name}: {id} is inserted before {}, which it requires via {}",
                fk.references,
                fk.name
            );
        }
        seen.insert(id.clone());
    }

    let coverage = Coverage::of(&schema, &order);

    // And now the test the plan called the whole project: generate rows for
    // this schema and make the database judge them.
    //
    // Every oracle test until this one ran against DDL written here, which
    // means it could only ever contain constructs someone here remembered to
    // handle. That is precisely how Crossfoot scored 17/22 on its author's own
    // corpus and 1/55 on real paper. These nine schemas were written by people
    // who had never heard of this tool.
    //
    // The tool's own default, not a smaller number chosen to be quick. Five
    // rows passed this gate while a `varchar(4)` unique column was still
    // colliding at fifty — the collision needed more rows than the gate was
    // asking for, which made the gate agree with itself and nothing else.
    let options = pgsow::emit::Options::flat(1, 50);
    let statements = pgsow::emit::statements(&schema, &verdict, &options);

    let (mut accepted, mut rejected) = (0usize, 0usize);
    let mut first_failures: Vec<String> = Vec::new();
    let mut by_code: std::collections::BTreeMap<String, usize> = Default::default();
    for statement in &statements {
        match client.batch_execute(statement) {
            Ok(()) => accepted += 1,
            Err(e) => {
                rejected += 1;
                let code = e
                    .as_db_error()
                    .map_or("?".into(), |d| d.code().code().to_string());
                *by_code.entry(code.clone()).or_insert(0usize) += 1;
                // A CHECK violation is the one that matters: it is data that
                // breaks a rule the schema stated, which is the exact failure
                // this project was built to refuse rather than commit.
                if matches!(code.as_str(), "23514" | "22000" | "23503") {
                    let head: String = statement
                        .lines()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(320)
                        .collect();
                    println!(
                        "      DOCTRINE {code}: {} | {head}",
                        e.as_db_error()
                            .map_or_else(|| e.to_string(), |d| d.message().into())
                    );
                }
                if first_failures.len() < 3 {
                    let head: String = statement.lines().take(2).collect::<Vec<_>>().join(" ");
                    let head: String = head.chars().take(110).collect();
                    first_failures.push(format!(
                        "{}
        {head}",
                        e.as_db_error()
                            .map_or_else(|| e.to_string(), |d| d.message().into())
                    ));
                }
            }
        }
    }

    println!(
        "    generated: {} statements · {accepted} accepted · {rejected} REJECTED",
        statements.len()
    );
    for (code, n) in &by_code {
        // 23505 duplicate key · 23502 not-null · 23503 foreign key · 42804 type
        println!("      SQLSTATE {code}: {n}");
    }
    for failure in &first_failures {
        println!("      rejected: {failure}");
    }

    // The pre-registered gate, and it is zero rather than a percentage.
    //
    // It was reported rather than asserted for exactly one commit, because a
    // gate set to whatever the figure happened to be would have measured
    // nothing. The figure was 43 rejections in 1,370 statements. Four bugs
    // later it is none, so this is now a gate a regression trips.
    //
    // The whole thesis is that the database adjudicates: one row it refuses is
    // a failure, not a percentage to be pleased with.
    assert_eq!(
        rejected,
        0,
        "{name}: Postgres rejected {rejected} of {} generated statements. 
         The gate is zero — the database is the oracle, and a row it refuses 
         is a row this should have refused first.
{first_failures:#?}",
        statements.len()
    );

    coverage
}

#[test]
fn reach_against_real_schemas() {
    println!("\nreach on schemas this project did not write:");
    let mut covered = [0usize; 13];
    let mut schemas = 0usize;
    for source in corpus_shared::sources() {
        let coverage = measure(
            &source.name,
            &Path::new("tests/corpus").join(&source.file),
            source.max_lost_constraints,
        );
        schemas += 1;
        for (total, present) in covered.iter_mut().zip(coverage.flags()) {
            *total += usize::from(present);
        }
    }

    println!(
        "
  what {schemas} real schemas exercise, and how thinly:"
    );
    for (construct, count) in Coverage::NAMES.iter().zip(covered) {
        let thin = if count == 0 {
            "  <- ABSENT: tested only by DDL written here"
        } else if count == 1 {
            "  <- thin: one schema away from untested"
        } else {
            ""
        };
        println!("    {construct:<20} {count:>3}{thin}");
    }

    // Constructs no real schema exercises, admitted in writing.
    //
    // A construct with none behind it is tested only against DDL written here,
    // which is the mistake this corpus exists to prevent — so each one is
    // named, with why it is still absent. A construct that becomes absent
    // without being on this list fails the test.
    const KNOWN_ABSENT: [(&str, &str); 1] = [(
        "domain type",
        "PostgREST defines seven domains and uses them in functions and casts,          never as the type of a table column. Nothing in twenty real schemas          has a domain-typed column, so `ColumnType::Domain` is exercised only          by the oracle tests.",
    )];

    let unexpected: Vec<&str> = Coverage::NAMES
        .iter()
        .zip(covered)
        .filter(|(name, count)| {
            *count == 0 && !KNOWN_ABSENT.iter().any(|(known, _)| known == *name)
        })
        .map(|(name, _)| *name)
        .collect();
    assert!(
        unexpected.is_empty(),
        "no real schema exercises: {unexpected:?}. Either the corpus needs a 
         schema that does, or this is claiming to handle something nothing 
         has ever asked it to. If neither, say so in KNOWN_ABSENT."
    );

    // And a construct that stops being absent should stop being excused.
    for (name, _) in KNOWN_ABSENT {
        let index = Coverage::NAMES
            .iter()
            .position(|n| *n == name)
            .expect("a real name");
        assert_eq!(
            covered[index], 0,
            "{name} is covered now — take it out of KNOWN_ABSENT"
        );
    }
}

/// Every CHECK constraint the closed set does not recognise, dumped verbatim.
///
/// Ignored by default: it is a survey rather than an assertion, and the point
/// of it is to design the next widening of the closed set from what real
/// schemas actually write instead of from what seems likely. Run with
/// `cargo test --test corpus -- --ignored --nocapture survey`.
#[test]
#[ignore]
fn survey_the_checks_this_does_not_understand() {
    let (mut total, mut known) = (0usize, 0usize);
    for name in [
        "powerdns",
        "hasura",
        "kong",
        "harbor",
        "temporal",
        "postgrest",
        "synapse",
        "discourse",
        "gitlab",
        "lago",
        "sourcegraph",
        "sourcegraph_codeintel",
        "sourcegraph_insights",
        "plausible",
        "hexpm",
        "mattermost",
        "vaultwarden",
        "kratos",
    ] {
        let path = Path::new("tests/corpus").join(format!("{name}.sql"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let db = Db::start();
        let mut client = db.client();
        let schemas = corpus_shared::load(&mut client, &text);
        let Ok(schema) = pgsow::introspect::read(&mut client, &schemas) else {
            continue;
        };
        for table in schema.tables.values() {
            for check in &table.checks {
                total += 1;
                if matches!(
                    pgsow::checks::interpret(&check.definition),
                    pgsow::checks::Meaning::Unknown
                ) {
                    println!(
                        "UNKNOWN\t{name}\t{}\t{}",
                        table.id,
                        check.definition.replace('\n', " ")
                    );
                } else {
                    known += 1;
                }
            }
        }
    }
    println!(
        "TOTALS	{total} checks, {known} understood, {} not",
        total - known
    );
}

/// Is INSERT actually too slow, and could COPY even be used?
///
/// The plan mentioned "COPY for volume" and it was never built. Two questions
/// decide whether it should be, and both have answers rather than opinions:
/// how slow is INSERT at a volume anybody would ask for, and how many tables
/// could COPY serve at all. COPY carries raw data and no expressions, so any
/// column whose value is a subquery — every foreign key into a table with a
/// database-generated key — cannot go through it.
///
/// `cargo test --test corpus -- --ignored --nocapture volume`
#[test]
#[ignore]
fn volume_and_whether_copy_could_carry_it() {
    for name in ["discourse", "gitlab"] {
        let path = Path::new("tests/corpus").join(format!("{name}.sql"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let db = Db::start();
        let mut client = db.client();
        for schema_name in schemas_in(&text) {
            let _ =
                client.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema_name}\";"));
        }
        for statement in statements(&text) {
            let _ = client.batch_execute(&statement);
        }
        let Ok(schema) = pgsow::introspect::read(&mut client, &schemas_in(&text)) else {
            continue;
        };
        let order = pgsow::graph::order(&schema);
        let verdict = pgsow::classify::classify(&schema, &order);

        for rows in [50usize, 1000] {
            let options = pgsow::emit::Options::flat(1, rows);
            let started = std::time::Instant::now();
            let statements = pgsow::emit::statements(&schema, &verdict, &options);
            let generated = started.elapsed();
            let bytes: usize = statements.iter().map(|s| s.len()).sum();

            // How much of it COPY could carry. A statement holding a subquery
            // cannot become a COPY at all; the rest could.
            let with_subquery = statements.iter().filter(|s| s.contains("(SELECT ")).count();

            let started = std::time::Instant::now();
            let mut transaction = client.transaction().unwrap();
            let mut failed = 0;
            let mut first_error = String::new();
            for statement in &statements {
                // No break: stopping at the first failure would leave the
                // timing measuring however far it got, which is not the
                // number this claims to report.
                if let Err(e) = transaction.batch_execute(statement) {
                    failed += 1;
                    if first_error.is_empty() {
                        first_error = e
                            .as_db_error()
                            .map_or_else(
                                || e.to_string(),
                                |d| format!("{} | {}", d.code().code(), d.message()),
                            )
                            .chars()
                            .take(140)
                            .collect();
                    }
                    // A failed statement aborts the transaction, so the rest
                    // would fail for that reason alone. Start a clean one.
                    break;
                }
            }
            if failed > 0 {
                println!("      first failure: {first_error}");
            }
            let applied = started.elapsed();
            transaction.rollback().unwrap();

            println!(
                "  {name} @ {rows} rows: generate {:?}, apply {:?}, {} MB,                  {} statements, {with_subquery} need a subquery, {failed} failed",
                generated,
                applied,
                bytes / 1_048_576,
                statements.len(),
            );
        }
    }
}
