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

fn statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_body = false;

    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") || trimmed.is_empty() {
            continue;
        }
        // Function bodies contain semicolons that do not end a statement.
        if trimmed.contains("$$") {
            in_body = !in_body;
        }
        current.push_str(line);
        current.push('\n');
        if !in_body && trimmed.ends_with(';') {
            out.push(std::mem::take(&mut current));
        }
    }
    out
}

fn measure(name: &str, path: &Path) {
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
            || head.starts_with("CREATE SCHEMA"))
        {
            continue;
        }
        match client.batch_execute(&statement) {
            Ok(()) => applied += 1,
            Err(e) => {
                skipped += 1;
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
                if head.starts_with("ALTER TABLE") && head.contains("CONSTRAINT") {
                    lost_constraints += 1;
                    if lost_constraints <= 2 {
                        let why = e.to_string();
                        let why: String = why.chars().take(70).collect();
                        eprintln!("      a table is under-constrained: {why}");
                    }
                }
            }
        }
    }

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
                fk.references, fk.name
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
                let code = e.as_db_error().map_or("?".into(), |d| d.code().code().to_string());
                *by_code.entry(code.clone()).or_insert(0usize) += 1;
                // A CHECK violation is the one that matters: it is data that
                // breaks a rule the schema stated, which is the exact failure
                // this project was built to refuse rather than commit.
                if matches!(code.as_str(), "23514" | "22000" | "23503") {
                    let head: String =
                        statement.lines().next().unwrap_or("").chars().take(60).collect();
                    println!("      DOCTRINE {code}: {} | {head}",
                        e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().into()));
                }
                if first_failures.len() < 3 {
                    let head: String = statement.lines().take(2).collect::<Vec<_>>().join(" ");
                    let head: String = head.chars().take(110).collect();
                    first_failures.push(format!("{}
        {head}",
                        e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().into())));
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
        rejected, 0,
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
    for name in ["powerdns", "hasura", "kong", "harbor", "temporal",
                 "postgrest", "synapse", "discourse", "gitlab"] {
        measure(name, &Path::new("tests/corpus").join(format!("{name}.sql")));
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
    for name in ["powerdns", "hasura", "kong", "harbor", "temporal",
                 "postgrest", "synapse", "discourse", "gitlab"] {
        let path = Path::new("tests/corpus").join(format!("{name}.sql"));
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let db = Db::start();
        let mut client = db.client();
        for schema_name in schemas_in(&text) {
            let _ = client.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema_name}\";"));
        }
        for statement in statements(&text) {
            let _ = client.batch_execute(&statement);
        }
        let Ok(schema) = pgsow::introspect::read(&mut client, &schemas_in(&text)) else { continue };
        for table in schema.tables.values() {
            for check in &table.checks {
                total += 1;
                if matches!(
                    pgsow::checks::interpret(&check.definition),
                    pgsow::checks::Meaning::Unknown
                ) {
                    println!("UNKNOWN\t{}", check.definition.replace('\n', " "));
                } else {
                    known += 1;
                }
            }
        }
    }
    println!("TOTALS	{total} checks, {known} understood, {} not", total - known);
}
