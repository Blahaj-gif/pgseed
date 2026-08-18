//! Whether a trigger can refuse a row.
//!
//! A trigger is a rule about what may be written, and nothing else here could
//! see one. Discourse has four that raise on insert — `topic_id in
//! topic_timers is readonly`, and rows written to those tables are rejected
//! however carefully every constraint was satisfied.
//!
//! Refusing every table that carries an insert trigger would be correct and
//! very expensive: measured over the corpus there are **373 row-level insert
//! triggers on 333 tables**, 314 of them in GitLab. It would undo most of what
//! the last two widenings bought.
//!
//! So, the same move as `checks` and `indexes`: ask the narrower question.
//! **A trigger can only refuse a row if it raises**, and `RAISE` takes a
//! severity. Of the 39 trigger bodies in the corpus that mention it, GitLab's
//! say `RAISE WARNING 'Manually assigning ids is not allowed'` — which logs
//! and carries on. Only the error severities stop an insert.
//!
//! That is a closed set of five words rather than a reading of the body, and
//! it is checked by the thing that matters: the corpus gate now installs
//! triggers, so a body this misreads produces a row the database rejects and
//! the gate fails. Being wrong here is loud.
//!
//! **Known limit:** a trigger that raises *indirectly*, by calling another
//! function that raises, is not detected. Nothing in the corpus does it, and
//! the gate would catch it if something did.

/// Severities `RAISE` accepts that do not stop the statement.
///
/// Everything else — `EXCEPTION`, an explicit `SQLSTATE`, or a bare `RAISE`
/// re-raising inside a handler — aborts. `EXCEPTION` is also the default when
/// no severity is written at all, which is why the absence of a word from this
/// list is what counts rather than the presence of one.
const HARMLESS_SEVERITIES: [&str; 5] = ["NOTICE", "WARNING", "INFO", "LOG", "DEBUG"];

/// Whether this trigger makes the row unreasonable-about: it either refuses
/// the insert, or rewrites what was written.
///
/// Both matter and only the first was obvious. A trigger that assigns to `NEW`
/// stores something other than what was generated, so every constraint checked
/// against the generated value was checked against a value that is not there —
/// and GitLab supplied seven CHECK violations, a duplicate key and three
/// not-null violations to prove it, all on tables whose triggers cannot raise.
///
/// But only for a column this actually writes. GitLab has three hundred
/// triggers that do `NEW."id" := nextval(...)`, and `id` carries a sequence
/// default, so it is never written here and the trigger overwrites nothing
/// that was reasoned about. Refusing on any assignment at all cost three
/// hundred tables for no gain, which is the same trap as refusing every CHECK.
pub fn interferes(body: &str, written: &[String]) -> bool {
    if can_raise(body) {
        return true;
    }
    // A column filled from a sequence is handled rather than refused, so an
    // assignment to one is not interference.
    let from_sequence = filled_from_sequence(body);
    assigns_to_new(body)
        .iter()
        .filter(|c| !from_sequence.contains(c))
        .any(|c| written.iter().any(|w| w == c))
}

/// Tables this trigger body writes rows into.
///
/// Writing elsewhere matters in two different ways, and telling them apart is
/// what keeps this from refusing three hundred tables to prevent one failure.
///
/// **The target gets rows this did not write.** GitLab's
/// `custom_dashboard_search_data` gets one per dashboard, and a unique key
/// over the dashboard then collides with the row written for the same parent
/// here. So a table any trigger writes into cannot be filled.
///
/// **The write itself may fail.** Only when the target is a table that was
/// never read — a partitioned one, most often. GitLab copies
/// `merge_request_diff_commits` into a partitioned table and Postgres answers
/// `no partition of relation ... found for row`. A write into a table that
/// *was* read is exactly as safe as filling that table, so it costs nothing.
///
/// Names only, unqualified, and duplicates are fine — the caller matches them
/// against the tables it knows.
pub fn writes_to(body: &str) -> Vec<String> {
    let upper = body.to_uppercase();
    let mut out = Vec::new();
    for verb in ["INSERT INTO", "UPDATE", "DELETE FROM", "MERGE INTO"] {
        let mut from = 0usize;
        while let Some(at) = word_at(&upper[from..], verb).map(|i| from + i) {
            from = at + verb.len();
            // Only where the verb starts a statement. `SELECT ... FOR UPDATE`
            // locks rows and writes nothing, and reading the word after it as
            // a table name refused a table called `SKIP` — from
            // `FOR UPDATE SKIP LOCKED`.
            if !starts_a_statement(&upper[..at]) {
                continue;
            }
            let rest = upper[from..].trim_start();
            // `UPDATE SET` inside `ON CONFLICT DO UPDATE SET` names no table.
            if rest.starts_with("SET") {
                continue;
            }
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.' || *c == '"')
                .collect();
            let name = name
                .trim_matches('"')
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_string();
            if !name.is_empty() {
                out.push(name.to_lowercase());
            }
        }
    }
    out
}

/// Columns this trigger fills from a sequence, and nothing else.
///
/// `NEW."id" := nextval('t_id_seq'::regclass)` is a sequence default written
/// as a trigger, which is how GitLab gives three hundred tables their primary
/// key. Treating it as interference refused all three hundred; treating the
/// column as database-generated — never written here, read back with a
/// subquery when a child needs it — is both correct and what the schema
/// author meant.
///
/// Only where the whole right-hand side is the `nextval` call. Anything else
/// computed from it is a different rule and is not read.
pub fn filled_from_sequence(body: &str) -> Vec<String> {
    let upper = body.to_uppercase();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(at) = word_at(&upper[from..], "NEW.").map(|i| from + i) {
        from = at + 4;
        let rest = &upper[from..];
        let Some((column, consumed)) = column_name(rest) else {
            continue;
        };
        let after = rest[consumed..].trim_start();
        let Some(value) = after.strip_prefix(":=") else {
            continue;
        };
        // Up to the end of the statement, which is where the assignment ends.
        let value = value.split(';').next().unwrap_or("").trim();
        // The whole value, not merely the start of it: `nextval('s') * 2` is
        // a different rule and reading it as a sequence default would claim a
        // column holds something it does not.
        if is_only_a_call(value, "NEXTVAL") {
            out.push(column);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Whether the text is exactly one call to `name` and nothing more.
fn is_only_a_call(text: &str, name: &str) -> bool {
    let Some(rest) = text.trim().strip_prefix(name) else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix('(') else {
        return false;
    };
    let mut depth = 1i32;
    for (index, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return rest[index + 1..].trim().is_empty();
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether the body writes to `NEW`.
///
/// `NEW.updated_at := now()` and `NEW.id = nextval(...)` are both assignments;
/// `IF NEW.a = NEW.b` is a comparison and changes nothing. Told apart by
/// position rather than by parsing: an assignment is the first thing in its
/// statement, a comparison never is.
pub fn assigns_to_new(body: &str) -> Vec<String> {
    let upper = body.to_uppercase();
    let mut out = Vec::new();

    // `SELECT ... INTO NEW.col`, the third spelling and the one that cost the
    // most to find. GitLab fills a sharding key that way.
    let mut from = 0usize;
    while let Some(at) = word_at(&upper[from..], "INTO").map(|i| from + i) {
        from = at + 4;
        let rest = upper[from..].trim_start();
        let rest = rest.strip_prefix("STRICT").map_or(rest, str::trim_start);
        if let Some((column, _)) = rest.strip_prefix("NEW.").and_then(column_name) {
            out.push(column);
        }
    }

    // `NEW.col := ...`, and `NEW.col = ...` where it starts a statement.
    let mut from = 0usize;
    while let Some(at) = word_at(&upper[from..], "NEW.").map(|i| from + i) {
        from = at + 4;
        let rest = &upper[from..];
        // The length consumed, not the name's length: `"ID"` is four
        // characters and `id` is two, and using the second to skip the first
        // left the scan pointing at a quote — so GitLab's three hundred
        // `NEW."id" := nextval(...)` triggers read as assigning nothing.
        let Some((column, consumed)) = column_name(rest) else {
            continue;
        };
        let after = rest[consumed..].trim_start();
        let assigned = after.starts_with(":=")
            || (starts_a_statement(&upper[..at])
                && after.starts_with('=')
                && !after.starts_with("=="));
        if assigned {
            out.push(column);
        }
    }

    out.sort();
    out.dedup();
    out
}

/// The identifier at the start of the text, quoted or not, in lower case —
/// and how many bytes of the text it occupied, which is not the same number
/// once quotes are involved.
fn column_name(text: &str) -> Option<(String, usize)> {
    let leading = text.len() - text.trim_start().len();
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('"') {
        let name = rest.split('"').next()?;
        // The name, plus both quotes, plus whatever whitespace came first.
        return (!name.is_empty()).then(|| (name.to_lowercase(), leading + name.len() + 2));
    }
    let name: String = text
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then(|| (name.to_lowercase(), leading + name.len()))
}

/// Whether this trigger function body can stop an insert.
pub fn can_raise(body: &str) -> bool {
    let upper = body.to_uppercase();

    // `ASSERT` aborts on a false condition and takes no severity.
    if word_at(&upper, "ASSERT").is_some() {
        return true;
    }

    let mut from = 0usize;
    while let Some(at) = word_at(&upper[from..], "RAISE").map(|i| from + i) {
        let rest = upper[at + "RAISE".len()..].trim_start();
        // The next word decides it. A severity from the list logs and carries
        // on; anything else — `EXCEPTION`, `SQLSTATE`, a message string, or
        // nothing at all — stops the statement.
        let next: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        if !HARMLESS_SEVERITIES.contains(&next.as_str()) {
            return true;
        }
        from = at + "RAISE".len();
    }
    false
}

/// Whether what comes before a keyword leaves it at the start of a statement.
///
/// The one test that tells an assignment from a comparison, and a write from a
/// row lock, without parsing plpgsql.
fn starts_a_statement(before: &str) -> bool {
    let before = before.trim_end();
    before.is_empty()
        || before.ends_with(';')
        || ["BEGIN", "THEN", "ELSE", "LOOP", "DECLARE"]
            .iter()
            .any(|keyword| before.ends_with(keyword))
}

/// Where a word appears as a whole word, not as part of a longer identifier.
///
/// Without this, a function called `raise_limit` or a column named
/// `assertion_id` reads as a raise and refuses its table for nothing.
fn word_at(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(offset) = haystack[from..].find(needle) {
        let at = from + offset;
        let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
        let after = at + needle.len();
        // Only when the needle itself ends in a word character; `NEW.` already
        // carries its own boundary.
        let after_ok = !needle.ends_with(|c: char| is_word_byte(c as u8))
            || after >= bytes.len()
            || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `interferes` against a table that writes every column a body might
    /// touch, which is what the old yes-or-no behaviour amounted to.
    fn interferes_any(body: &str) -> bool {
        let written = assigns_to_new(body);
        interferes(body, &written)
    }

    #[test]
    fn an_exception_stops_an_insert_and_a_warning_does_not() {
        // Both shapes are in the corpus. Discourse raises; GitLab warns.
        assert!(can_raise(
            "BEGIN RAISE EXCEPTION 'Discourse: topic_id in topic_timers is readonly'; END"
        ));
        assert!(!can_raise(
            "BEGIN RAISE WARNING 'Manually assigning ids is not allowed'; RETURN NEW; END"
        ));
    }

    #[test]
    fn every_harmless_severity_is_recognised_and_nothing_else_is() {
        for severity in ["NOTICE", "WARNING", "INFO", "LOG", "DEBUG"] {
            assert!(!can_raise(&format!(
                "BEGIN RAISE {severity} 'x'; RETURN NEW; END"
            )));
            // Lower case is how most of them are actually written.
            assert!(!can_raise(&format!(
                "begin raise {} 'x'; return new; end",
                severity.to_lowercase()
            )));
        }
        for stopping in ["EXCEPTION", "SQLSTATE '22000'", ""] {
            assert!(
                can_raise(&format!("BEGIN RAISE {stopping} 'x'; END")),
                "{stopping}"
            );
        }
    }

    #[test]
    fn an_assert_stops_an_insert_too() {
        assert!(can_raise(
            "BEGIN ASSERT NEW.id IS NOT NULL; RETURN NEW; END"
        ));
    }

    #[test]
    fn a_body_that_rewrites_the_row_interferes_even_though_it_cannot_raise() {
        // The whole reason `interferes` exists rather than just `can_raise`.
        assert!(!can_raise("BEGIN NEW.updated_at := now(); RETURN NEW; END"));
        assert!(interferes_any(
            "BEGIN NEW.updated_at := now(); RETURN NEW; END"
        ));
        assert!(interferes_any(
            "BEGIN NEW.id = nextval('s'); RETURN NEW; END"
        ));
        // The third spelling, and the one GitLab uses to fill a sharding key.
        assert!(interferes_any(
            "BEGIN SELECT namespace_id INTO NEW.ns_id FROM parents              WHERE parents.id = NEW.parent_id; RETURN NEW; END"
        ));
        assert!(interferes_any(
            "BEGIN SELECT a INTO STRICT NEW.\"b\" FROM t; RETURN NEW; END"
        ));
        assert!(interferes_any(
            "begin
  new.state = 0;
  return new;
end"
        ));
    }

    #[test]
    fn a_body_that_writes_elsewhere_names_where() {
        assert_eq!(
            writes_to("BEGIN INSERT INTO audit (x) VALUES (1); RETURN NEW; END"),
            vec!["audit".to_string()]
        );
        assert_eq!(
            writes_to("BEGIN UPDATE public.counts SET n = n + 1; RETURN NEW; END"),
            vec!["counts".to_string()]
        );
        // Writing elsewhere is not interference on its own — the caller
        // decides, because only it knows whether the target was read.
        assert!(!interferes_any(
            "BEGIN INSERT INTO audit (x) VALUES (1); RETURN NEW; END"
        ));
        assert!(writes_to("BEGIN RETURN NEW; END").is_empty());
        // `FOR UPDATE` locks rows and writes nothing; reading the next word as
        // a table name would refuse a table for a row lock.
        assert!(
            writes_to("BEGIN PERFORM 1 FROM t WHERE id = NEW.id FOR UPDATE; RETURN NEW; END")
                .is_empty()
        );
        assert!(
            writes_to("BEGIN PERFORM 1 FROM t FOR UPDATE SKIP LOCKED; RETURN NEW; END").is_empty()
        );
        // `ON CONFLICT DO UPDATE SET` names no table of its own.
        assert_eq!(
            writes_to("BEGIN INSERT INTO a VALUES (1) ON CONFLICT DO UPDATE SET x = 1; END"),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn a_column_this_never_writes_can_be_overwritten_freely() {
        // GitLab has three hundred of these. `id` carries a sequence default,
        // so it is never written here and the trigger overwrites nothing that
        // was reasoned about.
        // A plain overwrite rather than a sequence, so the only question is
        // whether the column is one this writes.
        let body = "BEGIN NEW.\"cached_at\" := now(); RETURN NEW; END";
        assert_eq!(assigns_to_new(body), vec!["cached_at".to_string()]);
        assert!(!interferes(body, &["name".to_string()]));
        assert!(interferes(
            body,
            &["cached_at".to_string(), "name".to_string()]
        ));
    }

    #[test]
    fn a_sequence_default_written_as_a_trigger_is_handled_not_refused() {
        let body = "BEGIN NEW.\"id\" := nextval('t_id_seq'::regclass); RETURN NEW; END";
        assert_eq!(filled_from_sequence(body), vec!["id".to_string()]);
        // Even though `id` is a column this would otherwise write.
        assert!(!interferes(body, &["id".to_string()]));

        // Anything else computed from it is a different rule.
        let derived = "BEGIN NEW.id := nextval('s') * 2; RETURN NEW; END";
        assert!(filled_from_sequence(derived).is_empty());
        assert!(interferes(derived, &["id".to_string()]));
    }

    #[test]
    fn reading_new_is_not_writing_to_it() {
        // A comparison changes nothing, and refusing on it would cost the
        // several hundred tables whose triggers only ever look.
        assert!(!interferes_any(
            "BEGIN IF NEW.a = NEW.b THEN RETURN NULL; END IF; RETURN NEW; END"
        ));
        assert!(!interferes_any("BEGIN RETURN NEW; END"));
        // `INTO` a local variable is not `INTO NEW`.
        assert!(!interferes_any(
            "DECLARE n int; BEGIN SELECT count(*) INTO n FROM t; RETURN NEW; END"
        ));
    }

    #[test]
    fn an_ordinary_trigger_body_can_refuse_nothing() {
        // The commonest kind by far, and the reason refusing every table with
        // a trigger would have been so expensive.
        assert!(!can_raise("BEGIN NEW.updated_at = now(); RETURN NEW; END"));
        // Cannot raise — though it does interfere, by writing elsewhere.
        assert!(!can_raise(
            "BEGIN UPDATE counts SET n = n + 1 WHERE id = NEW.parent_id; RETURN NEW; END"
        ));
    }

    #[test]
    fn a_word_that_merely_contains_raise_is_not_one() {
        // A function called `raise_limit`, a column called `assertion_id`.
        assert!(!can_raise(
            "BEGIN NEW.x = raise_limit(NEW.y); RETURN NEW; END"
        ));
        assert!(!can_raise("BEGIN NEW.assertion_id = 1; RETURN NEW; END"));
        assert!(!can_raise("BEGIN NEW.reraised = false; RETURN NEW; END"));
    }

    #[test]
    fn a_body_that_warns_and_then_raises_still_raises() {
        // The scan must not stop at the first harmless one.
        assert!(can_raise(
            "BEGIN RAISE NOTICE 'about to check'; \
             IF NEW.x IS NULL THEN RAISE EXCEPTION 'no'; END IF; RETURN NEW; END"
        ));
    }
}
