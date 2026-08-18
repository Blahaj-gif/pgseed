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
//!     unread — but the parents themselves are simply absent.
//!
//! Both were found by the `volume` benchmark below, which applies the *whole*
//! dump rather than the filtered part, and both are named here rather than
//! left implicit. A gate measuring a database less constrained than the real
//! one has flattered this project twice already, and saying where it still
//! does is the only honest way to quote its number.

mod harness;

use std::path::Path;

use harness::Db;

/// Split a dump into statements on semicolons at end of line, skipping the
/// parts of a dump that are not DDL. Crude, and sufficient: a statement this
/// mis-splits simply fails and is skipped, which costs one table out of
/// hundreds rather than corrupting the measurement.
/// Schemas a dump puts its tables in, so they can be created first.
fn schemas_in(sql: &str) -> Vec<String> {
    let mut schemas: std::collections::BTreeSet<String> = ["public".to_string()].into();
    for line in sql.lines() {
        for marker in ["CREATE TABLE ", "CREATE TABLE IF NOT EXISTS "] {
            if let Some(rest) = line.trim_start().strip_prefix(marker) {
                if let Some((qualifier, _)) = rest.split_once('.') {
                    let name = qualifier.trim().trim_matches('"');
                    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        schemas.insert(name.to_string());
                    }
                }
            }
        }
    }
    schemas.into_iter().collect()
}

/// Every dollar-quote tag on a line, in order: `$$`, `$_$`, `$function$`.
///
/// A tag is a `$`, then letters, digits or underscores not starting with a
/// digit, then another `$`. Anything else that happens to contain a dollar —
/// `$1` in a function body, a price in a string — is not one.
fn dollar_tags(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < bytes.len() && (bytes[end] == b'_' || bytes[end].is_ascii_alphanumeric()) {
            end += 1;
        }
        let is_tag = end < bytes.len() && bytes[end] == b'$' && !bytes[index + 1].is_ascii_digit();
        if is_tag {
            out.push(&line[index..=end]);
            index = end + 1;
        } else {
            index += 1;
        }
    }
    out
}

fn statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_body = false;
    let mut in_comment = false;
    let mut open_tag: Option<String> = None;

    for line in sql.lines() {
        let trimmed = line.trim();

        // A `/* ... */` banner at the start of a line, which is where files
        // put their licence and their explanations. Hasura's opens with one,
        // and gluing it to the first statement made that statement begin with
        // `/*` rather than `CREATE` — so it failed the head filter, the
        // function it defined was never created, and six of Hasura's eight
        // tables failed with it.
        //
        // Only at the start of a line, and only outside a function body. A
        // stripper that tracked quotes across the whole file desynchronised on
        // the apostrophe in an ordinary `-- don't` comment and took PostgREST
        // from 73 tables to none, which is a worse bug than the one it fixed.
        if in_comment {
            if trimmed.contains("*/") {
                in_comment = false;
            }
            continue;
        }
        if !in_body && trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_comment = true;
            }
            continue;
        }

        if trimmed.starts_with("--") || trimmed.is_empty() {
            continue;
        }
        // Function bodies contain semicolons that do not end a statement, and
        // they are delimited by a *tag*: `$$`, but also `$_$` or `$function$`.
        // Testing for `$$` alone never saw GitLab's `$_$` bodies, so every
        // semicolon inside one cut the statement in half and Postgres reported
        // an unterminated dollar-quoted string.
        for tag in dollar_tags(trimmed) {
            match &open_tag {
                None => open_tag = Some(tag.to_string()),
                Some(current) if current == tag => open_tag = None,
                Some(_) => {}
            }
        }
        in_body = open_tag.is_some();
        current.push_str(line);
        current.push('\n');
        if !in_body && trimmed.ends_with(';') {
            out.push(std::mem::take(&mut current));
        }
    }
    out
}

fn measure(name: &str, path: &Path, max_lost: usize) {
    let Ok(sql) = std::fs::read_to_string(path) else {
        eprintln!("  {name}: not fetched, skipping");
        return;
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
        let head = statement.trim_start().to_uppercase();
        // Functions and extensions are not the subject, but a column default
        // that calls one takes its whole CREATE TABLE down with it, and a lost
        // table is a table this never got to be measured against.
        if !(head.starts_with("CREATE TABLE")
            || head.starts_with("ALTER TABLE")
            || head.starts_with("CREATE TYPE")
            || head.starts_with("CREATE DOMAIN")
            || head.starts_with("CREATE SEQUENCE")
            || head.starts_with("CREATE FUNCTION")
            || head.starts_with("CREATE OR REPLACE FUNCTION")
            || head.starts_with("CREATE EXTENSION")
            || head.starts_with("CREATE SCHEMA")
            // Unique indexes are a uniqueness requirement exactly like a
            // unique constraint, and skipping them made this measure a
            // database less constrained than the real one — which is the one
            // way a gate can flatter itself.
            || head.starts_with("CREATE INDEX")
            || head.starts_with("CREATE UNIQUE INDEX")
            // A trigger is a rule about what may be written, and Discourse has
            // one that raises on insert. Leaving them out made this measure a
            // database that could not refuse what the real one refuses — the
            // third time that has been true here, after unique indexes and
            // after a comment banner swallowed the statement following it.
            || head.starts_with("CREATE TRIGGER")
            || head.starts_with("CREATE OR REPLACE TRIGGER"))
        {
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
}

#[test]
fn reach_against_real_schemas() {
    println!("\nreach on schemas this project did not write:");
    // Name, and the number of constraints its replay is known to lose.
    for (name, max_lost) in [
        ("powerdns", 0),
        ("hasura", 0),
        ("kong", 0),
        ("harbor", 0),
        ("temporal", 0),
        ("postgrest", 2),
        ("synapse", 0),
        ("discourse", 3),
        ("gitlab", 2),
        // Six added after the harness was repaired rather than before it, so
        // that a new denominator and a newly-honest measurement did not land
        // together and make each other hard to read.
        ("lago", 0),
        ("sourcegraph", 0),
        ("sourcegraph_codeintel", 0),
        ("sourcegraph_insights", 0),
        ("plausible", 1),
        ("hexpm", 2),
        // Replayed migration directories rather than a snapshot of a finished
        // schema. A ceiling above zero is expected here and is measured: a
        // replay only reaches the real shape if every migration applies.
        ("mattermost", 3),
        ("vaultwarden", 0),
        ("kratos", 0),
        // Configured, and skipped cleanly until it is fetched — the listing
        // API allows sixty calls an hour without a token and they were spent
        // finding these.
        ("hydra", 99),
    ] {
        measure(
            name,
            &Path::new("tests/corpus").join(format!("{name}.sql")),
            max_lost,
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
