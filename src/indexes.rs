//! Reading an index definition, or admitting that it cannot be.
//!
//! An index is not obviously a constraint, and two of them are.
//!
//! A **unique** index is a uniqueness requirement every bit as binding as
//! `UNIQUE (name)`, and it lives in `pg_index` rather than `pg_constraint`.
//! There are 1,397 of them across the nine corpus schemas.
//!
//! An **expression** index constrains the data whether or not it enforces
//! uniqueness, because the expression is evaluated on every row inserted.
//! Discourse indexes `((data)::jsonb ->> 'display_username')` on a `varchar`
//! column; nothing about that index is unique, and an ordinary word written to
//! `data` fails to cast and the row is rejected. An index that cannot be
//! violated can still refuse a row.
//!
//! Those two facts ask *different questions*, and conflating them is what made
//! the first attempt at this refuse 783 of GitLab's 956 tables:
//!
//!   - For a **non-unique** expression index the only question is **can this
//!     expression fail?** It enforces nothing, so if it cannot fail it
//!     constrains nothing and may be ignored entirely.
//!   - For a **unique** one the question is the harder **can this expression
//!     be made distinct?**
//!
//! `lower(col)` answers both, which is why it is the whole of the first closed
//! set. It is total — no text input makes it fail — and every string this
//! project generates is already lowercase, so `lower(col)` *is* `col` and
//! making the column distinct makes the expression distinct.
//!
//! Measured against the corpus, `lower()` is 37 of the 86 index expressions
//! there are. No other shape is close.
//!
//! As with `checks`, this is a **closed set of exact shapes** and not an
//! expression parser. A definition matches one of the forms below structurally
//! or the index is refused with its own text quoted back.

/// What an index turned out to require, when that is beyond doubt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    /// Plain columns, no expression. A unique index over them is a unique key
    /// and a non-unique one asks nothing at all.
    Columns(Vec<String>),
    /// Every expression is `lower(col)`, over the columns named — in index
    /// order, with the plain columns kept where they sit. Those columns must
    /// also be generated in lower case, which they already are, and which
    /// `Lowercase` records rather than assuming.
    Lowered {
        columns: Vec<String>,
        lowered: Vec<String>,
    },
    /// Every key is an expression that **cannot fail**, but not one whose
    /// distinctness can be reasoned about.
    ///
    /// Enough for a non-unique index, which enforces nothing and rejects rows
    /// only when its expression refuses to evaluate. Not enough for a unique
    /// one, which is asking a question this cannot answer.
    Harmless,
    /// Anything else at all.
    Unknown,
}

/// Split a parenthesised, comma-separated list at the top level.
///
/// `a, lower(b), (c ->> 'x')` gives three parts. Splitting on every comma
/// would cut a two-argument call in half and read the halves as columns.
fn top_level_parts(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start, mut quoted) = (0i32, 0usize, false);
    for (index, ch) in text.char_indices() {
        match ch {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(text[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    out.push(text[start..].trim());
    out
}

/// The key columns of an index definition, as written between the outermost
/// parentheses after the access method.
///
/// Everything after them — `INCLUDE (...)`, `WHERE ...`, storage options — is
/// dropped. INCLUDE columns are stored and not part of what is made unique,
/// and a partial index's predicate only ever *narrows* which rows the rule
/// applies to, so treating it as applying to all of them is the strict
/// reading and the safe one.
fn key_list(definition: &str) -> Option<&str> {
    let open = definition
        .find(" USING ")
        .and_then(|at| definition[at..].find('(').map(|offset| at + offset))?;
    let mut depth = 0i32;
    let mut quoted = false;
    for (index, ch) in definition[open..].char_indices() {
        match ch {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(definition[open + 1..open + index].trim());
                }
            }
            _ => {}
        }
    }
    None
}

/// A bare or quoted column name, with any cast Postgres prints removed.
fn column(text: &str) -> Option<String> {
    let text = unwrap(text.trim());
    // A cast counts only when it is the outermost thing there is. `(name)::text`
    // is the column `name`, but `(data)::jsonb ->> 'x'` merely *starts* with a
    // cast and is an expression — one that rejects rows when the cast fails.
    // Taking the text before the first `::` reads it as the column `data` and
    // silently drops the rule, which is the exact failure this project exists
    // to prevent, and which this comment exists because a test caught.
    let text = match text.split_once("::") {
        Some((left, right)) => {
            // Only a cast that keeps distinctness. `(name)::text` is the
            // column `name` for every purpose here, but `(created_at)::date`
            // maps many timestamps onto one day — and reading it as the bare
            // column claims a unique index on it is satisfied by making
            // `created_at` distinct, which it is not.
            let target = right.trim().trim_end_matches("[]").to_ascii_lowercase();
            if !TOTAL_CASTS.contains(&target.as_str()) {
                return None;
            }
            unwrap(left.trim())
        }
        None => text,
    };
    if let Some(inner) = text.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(inner.replace("\"\"", "\""));
    }
    let ok = !text.is_empty()
        && text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then(|| text.to_string())
}

/// Strip matched outer parentheses, repeatedly. `pg_get_indexdef` prints an
/// expression wrapped in its own pair, and a cast adds another.
fn unwrap(text: &str) -> &str {
    let mut current = text.trim();
    loop {
        let Some(inner) = current.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
            return current;
        };
        let mut depth = 0i32;
        for ch in inner.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return current;
            }
        }
        if depth != 0 {
            return current;
        }
        current = inner.trim();
    }
}

/// The single argument of `lower(...)`, if that is exactly what this is.
fn lower_argument(text: &str) -> Option<&str> {
    let text = unwrap(text.trim());
    let inner = text.strip_prefix("lower")?.trim_start().strip_prefix('(')?;
    let inner = inner.strip_suffix(')')?;
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return None;
        }
    }
    (depth == 0).then_some(inner)
}

/// Strip the per-column modifiers an index key may carry.
///
/// `lower(email) text_pattern_ops`, `created_at DESC NULLS LAST`. An operator
/// class chooses which comparison functions the index uses and an ordering
/// says which way it is stored; neither says anything whatever about what may
/// be written. Only *known* modifiers are stripped — the ordering keywords and
/// a trailing word ending in `_ops`, which is the naming convention Postgres
/// uses for every one of them. Stripping any trailing word would eat the
/// `zone` from `(a)::timestamp with time zone`.
fn strip_modifiers(part: &str) -> &str {
    let mut text = part.trim();
    loop {
        let upper = text.to_ascii_uppercase();
        let trimmed = ["NULLS FIRST", "NULLS LAST", "ASC", "DESC"]
            .iter()
            .find(|suffix| upper.ends_with(*suffix))
            .map(|suffix| text[..text.len() - suffix.len()].trim());
        if let Some(shorter) = trimmed {
            text = shorter;
            continue;
        }
        let opclass = text
            .rsplit_once(char::is_whitespace)
            .filter(|(_, last)| {
                last.ends_with("_ops")
                    && last.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .map(|(head, _)| head.trim());
        match opclass {
            Some(shorter) => text = shorter,
            None => return text,
        }
    }
}

/// Functions that cannot fail, whatever they are handed.
///
/// This is the closed set that makes a non-unique expression index safe to
/// ignore. `lower` of any text is text; `split_part` past the end is the empty
/// string; an array subscript out of range is NULL. None of them can refuse a
/// row, so an index built on them cannot either.
///
/// Deliberately short. `to_date` is not here and never will be, because
/// `to_date('bogus', 'YYYY-MM-DD')` raises — and a function that raises on
/// some inputs is exactly the kind that rejects a row.
/// The SQL string delimiter, written as a code point so that quoting it
/// through three layers of tooling stops being a source of bugs.
const QUOTE: char = '\u{27}';

const TOTAL_FUNCTIONS: [&str; 17] = [
    "lower",
    "upper",
    "btrim",
    "ltrim",
    "rtrim",
    "split_part",
    "coalesce",
    "md5",
    "char_length",
    "length",
    "octet_length",
    "concat",
    "concat_ws",
    "abs",
    "greatest",
    "least",
    "jsonb_typeof",
];

/// Casts that cannot fail. Anything at all can be rendered as text; the other
/// direction — text to `jsonb`, to `integer`, to `date` — is where a cast
/// refuses a row, and Discourse has an index that does.
const TOTAL_CASTS: [&str; 4] = ["text", "varchar", "character varying", "citext"];

/// Whether an expression can be shown never to fail.
///
/// Structural, like everything else here: a column, a literal, a subscript, a
/// null test, a cast to text, or a call to one of the functions above with
/// arguments that are themselves total. No evaluation, and an unrecognised
/// function name is not total by default.
fn is_total(expression: &str) -> bool {
    let text = unwrap(expression.trim());
    if text.is_empty() {
        return false;
    }

    // A literal: a quoted string or a number.
    if text.starts_with(QUOTE) && text.ends_with(QUOTE) && text.len() >= 2 {
        return true;
    }
    if text
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        return true;
    }

    // A cast, which is only total in the one direction.
    if let Some((left, right)) = split_last_cast(text) {
        let target = right.trim().trim_end_matches("[]").to_ascii_lowercase();
        return TOTAL_CASTS.contains(&target.as_str()) && is_total(left);
    }

    // A null test. Total for anything, and it yields a boolean.
    for suffix in ["IS NOT NULL", "IS NULL"] {
        if let Some(inner) = text.strip_suffix(suffix) {
            return is_total(inner);
        }
    }

    // An array subscript. Out of range is NULL rather than an error.
    if let Some((base, index)) = text.strip_suffix(']').and_then(|t| t.rsplit_once('[')) {
        return is_total(base) && is_total(index);
    }

    // A plain column.
    if column(text).is_some() {
        return true;
    }

    // A call to a function that cannot fail, on arguments that cannot either.
    if let Some((name, arguments)) = call_parts(text) {
        return TOTAL_FUNCTIONS.contains(&name.to_ascii_lowercase().as_str())
            && top_level_parts(arguments).iter().all(|a| is_total(a));
    }

    false
}

/// Split off a trailing `::type` at the top level, if there is one.
fn split_last_cast(text: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let mut quoted = false;
    let mut found = None;
    let bytes = text.as_bytes();
    for index in 0..bytes.len() {
        match bytes[index] {
            b if b == QUOTE as u8 => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            b':' if !quoted && depth == 0 && text[index..].starts_with("::") => {
                found = Some(index);
            }
            _ => {}
        }
    }
    found.map(|at| (&text[..at], &text[at + 2..]))
}

/// A call as (name, arguments), if the text is exactly one.
fn call_parts(text: &str) -> Option<(&str, &str)> {
    let open = text.find('(')?;
    let name = text[..open].trim();
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || !text.trim_end().ends_with(')')
    {
        return None;
    }
    let inner = &text[open + 1..text.trim_end().len() - 1];
    // The parentheses must balance inside, or `f(a) + g(b)` parses as a call.
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return None;
        }
    }
    (depth == 0).then_some((name, inner))
}

/// Read what an index requires, from the text `pg_get_indexdef` returned.
pub fn interpret(definition: &str) -> Requirement {
    let Some(keys) = key_list(definition) else {
        return Requirement::Unknown;
    };

    let mut columns = Vec::new();
    let mut lowered = Vec::new();
    for part in top_level_parts(keys) {
        let part = strip_modifiers(part);
        if let Some(name) = column(part) {
            columns.push(name);
            continue;
        }
        // The one expression in the set. `lower(x)` cannot fail on text, and
        // every string generated here is already lower case, so it is the
        // column itself as far as distinctness goes.
        if let Some(name) = lower_argument(part).and_then(column) {
            columns.push(name.clone());
            lowered.push(name);
            continue;
        }
        // Not a shape whose distinctness can be reasoned about. It may still
        // be one that cannot fail, which is all a non-unique index needs.
        return if top_level_parts(keys)
            .iter()
            .all(|p| is_total(strip_modifiers(p)))
        {
            Requirement::Harmless
        } else {
            Requirement::Unknown
        };
    }

    if columns.is_empty() {
        return Requirement::Unknown;
    }
    if lowered.is_empty() {
        Requirement::Columns(columns)
    } else {
        Requirement::Lowered { columns, lowered }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(definition: &str) -> Requirement {
        interpret(definition)
    }

    fn columns(names: &[&str]) -> Requirement {
        Requirement::Columns(names.iter().map(|n| n.to_string()).collect())
    }

    #[test]
    fn plain_columns_are_read_in_index_order() {
        assert_eq!(
            on("CREATE UNIQUE INDEX i ON public.t USING btree (a, b)"),
            columns(&["a", "b"])
        );
        assert_eq!(
            on("CREATE INDEX i ON public.t USING btree (a)"),
            columns(&["a"])
        );
    }

    #[test]
    fn a_quoted_or_cast_column_is_still_a_column() {
        assert_eq!(
            on("CREATE UNIQUE INDEX i ON t USING btree (\"position\")"),
            columns(&["position"])
        );
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (((name)::text))"),
            columns(&["name"])
        );
    }

    #[test]
    fn include_and_where_and_storage_are_not_part_of_the_key() {
        // INCLUDE columns are stored, not made unique. A partial predicate
        // only narrows which rows the rule covers, so ignoring it is the
        // strict reading: rows all distinct satisfy a rule that only some
        // of them be.
        assert_eq!(
            on("CREATE UNIQUE INDEX i ON t USING btree (a) INCLUDE (b, c)"),
            columns(&["a"])
        );
        assert_eq!(
            on("CREATE UNIQUE INDEX i ON t USING btree (a) WHERE (b IS NULL)"),
            columns(&["a"])
        );
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (a) WITH (fillfactor='90')"),
            columns(&["a"])
        );
    }

    #[test]
    fn lower_is_the_one_expression_understood() {
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (lower((email)::text))"),
            Requirement::Lowered {
                columns: vec!["email".into()],
                lowered: vec!["email".into()],
            }
        );
        // Beside a plain column, and the order is kept: the tuple this makes
        // unique is (namespace_id, name), not (name, namespace_id).
        assert_eq!(
            on("CREATE UNIQUE INDEX i ON t USING btree (namespace_id, lower(name))"),
            Requirement::Lowered {
                columns: vec!["namespace_id".into(), "name".into()],
                lowered: vec!["name".into()],
            }
        );
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (lower(a), lower(b))"),
            Requirement::Lowered {
                columns: vec!["a".into(), "b".into()],
                lowered: vec!["a".into(), "b".into()],
            }
        );
    }

    #[test]
    fn an_operator_class_or_an_ordering_is_not_part_of_the_key() {
        // Both choose how the index is built and neither says anything about
        // what may be written. Seven of these in the corpus were refusing
        // their tables over a tuning decision.
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (name text_pattern_ops)"),
            columns(&["name"])
        );
        assert_eq!(
            on("CREATE UNIQUE INDEX i ON t USING btree (a, b varchar_pattern_ops)"),
            columns(&["a", "b"])
        );
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (created_at DESC NULLS LAST)"),
            columns(&["created_at"])
        );
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (lower((email)::text) text_pattern_ops)"),
            Requirement::Lowered {
                columns: vec!["email".into()],
                lowered: vec!["email".into()],
            }
        );
    }

    #[test]
    fn a_type_name_with_spaces_is_not_mistaken_for_a_modifier() {
        // The point of this one is the *stripper*: `zone` must not be taken
        // for an operator class, and it is not, because only words ending in
        // `_ops` are. The result is `Unknown` rather than a column because a
        // cast to a timestamp does not keep distinctness, which is a separate
        // and equally deliberate decision.
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (((a)::timestamp with time zone))"),
            Requirement::Unknown
        );
        // The stripper working, shown on a cast that does keep distinctness.
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (((a)::character varying) DESC)"),
            columns(&["a"])
        );
    }

    #[test]
    fn everything_outside_the_set_is_unknown() {
        // A cast away from text can fail, and a failing cast rejects the row —
        // which is the whole reason expression indexes are read at all. An
        // operator is not a call, and a function not on the list is not
        // assumed to be total just because it looks harmless.
        for definition in [
            "CREATE INDEX i ON t USING btree ((((data)::jsonb ->> 'x'::text)))",
            "CREATE INDEX i ON t USING btree (((a + b)))",
            "CREATE INDEX i ON t USING gin (to_tsvector('english'::regconfig, a))",
            "CREATE INDEX i ON t USING btree (jsonb_array_length(COALESCE(a, '[]')))",
            "CREATE INDEX i ON t USING btree (to_date(a, 'YYYY-MM-DD'::text))",
            "CREATE INDEX i ON t USING btree (((a)::integer))",
        ] {
            assert_eq!(interpret(definition), Requirement::Unknown, "{definition}");
        }
    }

    #[test]
    fn an_expression_that_cannot_fail_is_harmless_when_nothing_is_unique() {
        // These constrain nothing at all on a non-unique index: an array
        // subscript out of range is NULL, `split_part` past the end is the
        // empty string, a null test is a boolean. All three refuse GitLab's
        // most central tables — namespaces, users and issues — and between
        // them they cost hundreds of tables to contagion.
        for definition in [
            "CREATE INDEX i ON t USING btree ((ids[1]), created_at)",
            "CREATE INDEX i ON t USING btree (a, ((b IS NULL)))",
            "CREATE INDEX i ON t USING btree (lower(split_part((email)::text, '@'::text, 2)), id)",
            "CREATE INDEX i ON t USING btree (upper(a))",
            "CREATE INDEX i ON t USING btree (md5(a))",
            "CREATE INDEX i ON t USING btree (coalesce(a, ''::text))",
        ] {
            assert_eq!(interpret(definition), Requirement::Harmless, "{definition}");
        }
    }

    #[test]
    fn a_cast_that_loses_distinctness_is_not_a_column() {
        // `(created_at)::date` maps many timestamps onto one day, so a unique
        // index on it is *not* satisfied by making `created_at` distinct.
        // Reading it as the bare column would have claimed otherwise.
        assert_eq!(
            interpret("CREATE UNIQUE INDEX i ON t USING btree (((created_at)::date))"),
            Requirement::Unknown
        );
        // A cast to text keeps distinctness and stays a column.
        assert_eq!(
            interpret("CREATE UNIQUE INDEX i ON t USING btree (((name)::text))"),
            columns(&["name"])
        );
    }

    #[test]
    fn a_comma_inside_a_call_does_not_split_the_list() {
        // `to_tsvector('english', body)` is one key, not two, and reading it
        // as two would find a column called `body` and index it.
        assert_eq!(
            on("CREATE INDEX i ON t USING gin (to_tsvector('english', body))"),
            Requirement::Unknown
        );
    }

    #[test]
    fn a_comma_inside_a_string_does_not_split_the_list_either() {
        assert_eq!(
            on("CREATE INDEX i ON t USING btree (a) WHERE (b = 'x,y'::text)"),
            columns(&["a"])
        );
    }

    #[test]
    fn something_that_is_not_an_index_definition_is_unknown() {
        assert_eq!(interpret("CHECK ((a > 0))"), Requirement::Unknown);
        assert_eq!(interpret(""), Requirement::Unknown);
        assert_eq!(
            interpret("CREATE INDEX i ON t USING btree ()"),
            Requirement::Unknown
        );
    }
}

#[cfg(test)]
mod real_text {
    //! Exactly what `pg_get_indexdef` printed for the index that duplicated.
    use super::*;

    #[test]
    fn the_gitlab_uid_index_reads_as_one_column() {
        assert_eq!(
            interpret(
                "CREATE UNIQUE INDEX index_oauth_applications_on_uid \
                 ON public.oauth_applications USING btree (uid)"
            ),
            Requirement::Columns(vec!["uid".into()])
        );
    }
}
