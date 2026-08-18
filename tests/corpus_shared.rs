//! The parts of the corpus harness that more than one test needs.
//!
//! Extracted rather than copied: a second reader of these dumps that split
//! them slightly differently would answer a slightly different question, and
//! the answers would be compared as though they were the same.

#![allow(dead_code)]

/// Every schema in the corpus, in the order they are measured.
pub const NAMES: &[&str] = &[
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
    "hydra",
];

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
/// Split a dump into statements on semicolons at end of line. Crude, and
/// sufficient: a statement this mis-splits simply fails and is skipped, which
/// costs one table out of hundreds rather than corrupting the measurement.
pub fn statements(sql: &str) -> Vec<String> {
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
