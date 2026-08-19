//! The parts of the corpus harness that more than one test needs.
//!
//! Extracted rather than copied: a second reader of these dumps that split
//! them slightly differently would answer a slightly different question, and
//! the answers would be compared as though they were the same.

#![allow(dead_code)]

/// One schema of the corpus, as `sources.json` describes it.
///
/// Read from the manifest rather than repeated here. The manifest already had
/// to carry the URL and the licence; carrying the ceiling too means a schema
/// is described in exactly one place, and there is no second list to keep in
/// step by hand.
#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub file: String,
    /// Constraints this schema's replay is known to lose. Measured.
    pub max_lost_constraints: usize,
}

/// Every schema in the corpus, in the order the manifest lists them.
pub fn sources() -> Vec<Source> {
    let text = std::fs::read_to_string("tests/corpus/sources.json")
        .expect("the corpus manifest is part of the repository");
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("the manifest is valid JSON");
    parsed["schemas"]
        .as_array()
        .expect("the manifest lists schemas")
        .iter()
        .map(|entry| Source {
            name: entry["name"].as_str().expect("a name").to_string(),
            file: entry["file"].as_str().expect("a file").to_string(),
            max_lost_constraints: entry["max_lost_constraints"].as_u64().unwrap_or(0) as usize,
        })
        .collect()
}

/// Schemas a dump puts its tables in, so they can be created first.
pub fn schemas_in(sql: &str) -> Vec<String> {
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
pub fn dollar_tags(line: &str) -> Vec<&str> {
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
/// Split a dump into statements on semicolons.
///
/// On semicolons, not on lines that happen to end with one. listmonk writes
/// `DROP TYPE IF EXISTS x CASCADE; CREATE TYPE x AS ENUM (...);` on a single
/// line, and treating that as one statement meant its head was `DROP` — so the
/// filter dropped it, the enum was never created, and every table using one
/// failed. Twelve of its sixteen tables were missing for that reason.
///
/// Single quotes and dollar-quoted bodies are tracked, so a semicolon inside a
/// string or a function body does not end anything.
pub fn statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut open_tag: Option<String> = None;
    let mut quoted = false;
    let mut in_comment = false;

    for line in sql.lines() {
        let trimmed = line.trim();

        // A `/* ... */` banner at the start of a line, which is where files
        // put their licence and their explanations. Only at the start, and
        // only outside a body: a stripper that tracked quotes across the whole
        // file desynchronised on the apostrophe in an ordinary `-- don't`.
        if in_comment {
            if trimmed.contains("*/") {
                in_comment = false;
            }
            continue;
        }
        if open_tag.is_none() && !quoted && trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_comment = true;
            }
            continue;
        }
        if open_tag.is_none() && !quoted && (trimmed.starts_with("--") || trimmed.is_empty()) {
            continue;
        }

        for tag in dollar_tags(line) {
            match &open_tag {
                None => open_tag = Some(tag.to_string()),
                Some(open) if open == tag => open_tag = None,
                Some(_) => {}
            }
        }

        // Walk the line, ending a statement at each top-level semicolon.
        let mut rest = line;
        while open_tag.is_none() {
            let Some(at) = next_semicolon(rest, &mut quoted) else {
                break;
            };
            current.push_str(&rest[..=at]);
            let statement = std::mem::take(&mut current);
            if !statement.trim().is_empty() {
                out.push(statement);
            }
            rest = &rest[at + 1..];
        }
        current.push_str(rest);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// The next semicolon outside a string literal, advancing the quote state over
/// everything it passes.
fn next_semicolon(text: &str, quoted: &mut bool) -> Option<usize> {
    for (index, ch) in text.char_indices() {
        match ch {
            '\u{27}' => *quoted = !*quoted,
            ';' if !*quoted => return Some(index),
            _ => {}
        }
    }
    None
}

/// Load a schema dump, and leave the session the way a real one would be.
///
/// Four of the twenty corpus files are `pg_dump` output, and `pg_dump` writes
/// `SELECT pg_catalog.set_config('search_path', '', false)` near the top. The
/// `false` makes it a session setting rather than a transaction one, so it
/// outlives the load and applies to everything the test does afterwards.
///
/// That was not harmless. Plausible's `sites` table carries a trigger whose
/// body says `SELECT 1 FROM sites` without qualifying it, which is ordinary
/// and correct — and with an empty search path it fails with `42P01 relation
/// "sites" does not exist`. Every measurement of that table was therefore
/// measuring the harness. A real connection has `"$user", public`, so the
/// search path is put back to something real before anything is asked of the
/// database.
/// Whether a statement is the kind that shapes a schema.
///
/// The corpus gate has always filtered to this, and the other harnesses did
/// not — so they were loading the seed rows a production dump carries, and
/// measuring a different database. Synapse ships
/// `INSERT INTO appservice_stream_position VALUES ('X', 0)`, whose table can
/// hold exactly one row; with that row already there, a tool asking for one
/// more is correct and the insert still fails. Two harnesses that disagree
/// about what is in the database produce two numbers nobody can compare, which
/// is how a two-table discrepancy went into a README.
pub fn shapes_the_schema(statement: &str) -> bool {
    let head = statement.trim_start().to_uppercase();
    [
        "CREATE TABLE",
        "ALTER TABLE",
        "CREATE TYPE",
        "CREATE DOMAIN",
        "CREATE SEQUENCE",
        "CREATE FUNCTION",
        "CREATE OR REPLACE FUNCTION",
        "CREATE EXTENSION",
        "CREATE SCHEMA",
        "CREATE INDEX",
        "CREATE UNIQUE INDEX",
        "CREATE TRIGGER",
        "CREATE OR REPLACE TRIGGER",
    ]
    .iter()
    .any(|allowed| head.starts_with(allowed))
}

/// Supply a function whose extension would not install, where that can be done
/// exactly.
///
/// The Postgres `postgresql_embedded` ships has `uuid-ossp`'s control file on
/// Linux and not its library, so `CREATE EXTENSION` fails there and succeeds
/// here. hex.pm defaults a primary key to `uuid_generate_v4()`, so its
/// `CREATE TABLE users` then fails and twenty constraints on that table are
/// counted as lost — a fact about the build reported as a fact about the
/// schema, and the corpus became twenty schemas on one machine and nineteen on
/// another.
///
/// `gen_random_uuid()` has been in core since Postgres 13 and produces the same
/// thing: a random 128-bit value with the version bits set. So the function is
/// supplied and the extension is not.
///
/// **This is deliberately not a retry-whatever-failed.** Making a schema load
/// is exactly the pressure that produces a flattering measurement, so:
/// one named function, written out by hand, whose semantics are known; no
/// constraint of any kind is changed; and everything else — `vector`,
/// `pg_partman`, anything with behaviour of its own — gets nothing, and a
/// schema needing one stays unmeasured and says so.
pub fn shim_for(client: &mut postgres::Client, failed: &str) -> Option<&'static str> {
    let head = failed.trim_start().to_uppercase();
    if !head.starts_with("CREATE EXTENSION") || !head.contains("UUID-OSSP") {
        return None;
    }
    // Schema-qualified because hex.pm calls it that way and Hydra does not;
    // `public` is on the search path for the second case.
    client
        .batch_execute(
            "CREATE OR REPLACE FUNCTION public.uuid_generate_v4() RETURNS uuid              LANGUAGE sql AS 'SELECT gen_random_uuid()'",
        )
        .ok()
        .map(|()| "uuid-ossp")
}

pub fn load(client: &mut postgres::Client, sql: &str) -> Vec<String> {
    let schemas = schemas_in(sql);
    for name in &schemas {
        let _ = client.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{name}\";"));
    }
    for statement in statements(sql).into_iter().filter(|s| shapes_the_schema(s)) {
        if client.batch_execute(&statement).is_err() {
            shim_for(client, &statement);
        }
    }

    let path: Vec<String> = schemas
        .iter()
        .map(|name| format!("\"{name}\""))
        .chain(["public".to_string()])
        .collect();
    let _ = client.batch_execute(&format!("SET search_path TO {};", path.join(", ")));
    schemas
}
