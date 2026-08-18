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
    Lowered { columns: Vec<String>, lowered: Vec<String> },
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
    let open = definition.find(" USING ").and_then(|at| {
        definition[at..].find('(').map(|offset| at + offset)
    })?;
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
            let right = right.trim();
            let type_name = !right.is_empty()
                && right
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ');
            if !type_name {
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
        && text.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
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

/// Read what an index requires, from the text `pg_get_indexdef` returned.
pub fn interpret(definition: &str) -> Requirement {
    let Some(keys) = key_list(definition) else {
        return Requirement::Unknown;
    };

    let mut columns = Vec::new();
    let mut lowered = Vec::new();
    for part in top_level_parts(keys) {
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
        return Requirement::Unknown;
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
    fn everything_outside_the_set_is_unknown() {
        // Each of these is a real rule, and none of them is approximated. The
        // casts are the reason the first attempt at this existed at all: a
        // cast can fail, and a failing cast rejects the row.
        for definition in [
            "CREATE INDEX i ON t USING btree ((((data)::jsonb ->> 'x'::text)))",
            "CREATE INDEX i ON t USING btree ((ids[1]), created_at)",
            "CREATE INDEX i ON t USING btree (a, ((b IS NULL)))",
            "CREATE INDEX i ON t USING btree (upper(a))",
            "CREATE INDEX i ON t USING btree (md5(a))",
            "CREATE INDEX i ON t USING btree ((a + b))",
            "CREATE INDEX i ON t USING gin (to_tsvector('english'::regconfig, a))",
            "CREATE INDEX i ON t USING btree (jsonb_array_length(COALESCE(a, '[]')))",
        ] {
            assert_eq!(interpret(definition), Requirement::Unknown, "{definition}");
        }
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
        assert_eq!(interpret("CREATE INDEX i ON t USING btree ()"), Requirement::Unknown);
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
