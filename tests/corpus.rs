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

    let schema = pgsow::introspect::read(&mut client, &["public".to_string()])
        .expect("introspection failed on a real schema");
    let order = pgsow::graph::order(&schema);
    let verdict = pgsow::classify::classify(&schema, &order);

    // Why each refusal happened, which is more useful than the total.
    let (mut checks, mut types, mut cycles, mut inherited) = (0, 0, 0, 0);
    for (_, reasons) in &verdict.refused {
        for reason in reasons {
            match reason {
                pgsow::classify::Refusal::CheckConstraint { .. } => checks += 1,
                pgsow::classify::Refusal::UnsupportedType { .. } => types += 1,
                pgsow::classify::Refusal::UnbreakableCycle { .. } => cycles += 1,
                pgsow::classify::Refusal::DependsOnRefused { .. } => inherited += 1,
            }
        }
    }

    println!(
        "\n  {name}\n    {applied} applied, {skipped} skipped, {lost_constraints} of them constraints a live table has now lost\n    \
         {} tables · {} fillable · {} refused · REACH {:.0}%\n    \
         refused because: {checks} CHECK · {types} unsupported type · \
         {cycles} unbreakable cycle · {inherited} depends on a refused table",
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
}

#[test]
fn reach_against_real_schemas() {
    println!("\nreach on schemas this project did not write:");
    for name in ["synapse", "gitlab"] {
        measure(name, &Path::new("tests/corpus").join(format!("{name}.sql")));
    }
}
