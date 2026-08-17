//! Understanding a CHECK constraint, or admitting that it does not.
//!
//! The plan for this tool said CHECK constraints would be read and never
//! solved, because a partial solver that handles `total > 0` and mishandles
//! `total > 0 OR status = 'void'` is worse than none — the cases it gets wrong
//! look exactly like the ones it gets right.
//!
//! That is still true of *evaluating expressions*. It turned out to be far too
//! blunt as a policy, and the measurement said so. Against GitLab's schema —
//! 955 tables, not written by anyone here — refusing every table carrying a
//! CHECK gave a reach of **8%**. Against Synapse it gave 96%. The difference
//! is not complexity; it is that GitLab writes a great many constraints of two
//! extremely simple shapes:
//!
//! ```text
//!   2537 CHECK expressions in gitlab structure.sql
//!   1657  char_length(col) <= N        65%
//!    414  col IS NOT NULL              16%
//!    106  num_nonnulls(...)
//!    360  everything else
//! ```
//!
//! **81% of them are two forms this already satisfies by construction.** A
//! length limit is what `varchar(n)` means, and a NOT NULL written as a CHECK
//! is a column this never writes NULL into anyway.
//!
//! So this module recognises a **closed set of exact shapes** and refuses
//! everything else. That is not the expression parser the plan rejected: there
//! is no evaluation, no simplification and no reasoning about operators. A
//! definition either matches one of the literal forms below, character for
//! character in structure, or it is `Unknown` and its table is refused.
//! Widening the set means adding a shape here and re-running the corpus, which
//! is the point at which the cost of being wrong is visible.

/// What a CHECK constraint turned out to mean, when it is one of the few
/// shapes that can be recognised beyond doubt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Meaning {
    /// `char_length(col) <= N`, or `length(col) <= N`. A limit on generated
    /// text, identical in effect to `varchar(N)`.
    LengthLimit { column: String, max: i32 },
    /// `col IS NOT NULL`. Already guaranteed: this never writes NULL into a
    /// column it is generating a value for.
    NotNull { column: String },
    /// Anything at all that is not exactly one of the above.
    Unknown,
}

/// Strip one layer of surrounding parentheses, repeatedly.
///
/// `pg_get_constraintdef` is generous with them: a constraint written
/// `CHECK (char_length(name) <= 255)` comes back as
/// `CHECK ((char_length(name) <= 255))`.
fn unwrap_parens(text: &str) -> &str {
    let mut current = text.trim();
    loop {
        let Some(inner) = current.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
            return current;
        };
        // Only unwrap when the parentheses are genuinely a matched outer pair.
        // `(a) OR (b)` starts with `(` and ends with `)` and stripping them
        // would produce the nonsense `a) OR (b`.
        let mut depth = 0i32;
        let mut balanced = true;
        for (index, ch) in inner.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 && index != inner.len() {
                        balanced = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !balanced || depth != 0 {
            return current;
        }
        current = inner.trim();
    }
}

/// A bare column name: what Postgres prints for an unquoted identifier, or a
/// quoted one with the quotes still on. Anything containing an operator, a
/// call or a cast is not a column and must not be treated as one.
fn column_name(text: &str) -> Option<String> {
    let text = unwrap_parens(text);
    if let Some(stripped) = text.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(stripped.replace("\"\"", "\""));
    }
    let ok = !text.is_empty()
        && text.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then(|| text.to_string())
}

/// Read a constraint definition, and say what it means only when that is
/// beyond doubt.
///
/// `definition` is what `pg_get_constraintdef` returned, verbatim.
pub fn interpret(definition: &str) -> Meaning {
    let body = definition.trim();
    let Some(rest) = body.strip_prefix("CHECK") else {
        return Meaning::Unknown;
    };
    // A constraint may be marked NOT VALID, which says nothing about what it
    // requires of new rows — every row this writes still has to satisfy it.
    let rest = rest.trim().trim_end_matches("NOT VALID").trim();
    let expression = unwrap_parens(rest);

    if let Some(column) = expression.strip_suffix("IS NOT NULL").map(column_name) {
        if let Some(column) = column {
            return Meaning::NotNull { column };
        }
    }

    if let Some((left, right)) = expression.split_once("<=") {
        let left = unwrap_parens(left);
        for call in ["char_length", "length"] {
            let Some(inner) = left
                .strip_prefix(call)
                .map(str::trim)
                .and_then(|s| s.strip_prefix('('))
                .and_then(|s| s.strip_suffix(')'))
            else {
                continue;
            };
            let Some(column) = column_name(inner) else { continue };
            // The bound is written `255` or `(255)::integer` depending on how
            // it was declared. Anything that is not a plain integer after the
            // cast is stripped is not a bound this understands.
            let bound = unwrap_parens(right)
                .split("::")
                .next()
                .map(|s| unwrap_parens(s).trim().to_string());
            if let Some(max) = bound.and_then(|b| b.parse::<i32>().ok()) {
                if max > 0 {
                    return Meaning::LengthLimit { column, max };
                }
            }
        }
    }

    Meaning::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_length_limit_is_recognised_in_the_form_postgres_prints_it() {
        // What `pg_get_constraintdef` actually returns, doubled parens and all.
        assert_eq!(
            interpret("CHECK ((char_length(name) <= 255))"),
            Meaning::LengthLimit { column: "name".into(), max: 255 }
        );
        assert_eq!(
            interpret("CHECK ((length(title) <= 64))"),
            Meaning::LengthLimit { column: "title".into(), max: 64 }
        );
    }

    #[test]
    fn a_bound_written_as_a_cast_is_still_a_bound() {
        assert_eq!(
            interpret("CHECK ((char_length(path) <= (255)::integer))"),
            Meaning::LengthLimit { column: "path".into(), max: 255 }
        );
    }

    #[test]
    fn a_not_null_written_as_a_check_is_recognised() {
        // 414 of GitLab's constraints are this, usually left behind by a
        // migration that adds NOT NULL without a table rewrite.
        assert_eq!(
            interpret("CHECK ((description IS NOT NULL))"),
            Meaning::NotNull { column: "description".into() }
        );
    }

    #[test]
    fn not_valid_does_not_change_what_a_constraint_requires() {
        // NOT VALID means existing rows were not checked. New ones still are,
        // and every row this writes is a new one.
        assert_eq!(
            interpret("CHECK ((char_length(name) <= 255)) NOT VALID"),
            Meaning::LengthLimit { column: "name".into(), max: 255 }
        );
    }

    #[test]
    fn a_quoted_column_keeps_its_real_name() {
        assert_eq!(
            interpret("CHECK ((char_length(\"user name\") <= 40))"),
            Meaning::LengthLimit { column: "user name".into(), max: 40 }
        );
    }

    #[test]
    fn anything_outside_the_closed_set_is_unknown() {
        // The whole safety of this module. Each of these is refused, and none
        // of them is approximated.
        for definition in [
            "CHECK ((total > (0)::numeric))",
            "CHECK (((total > 0) OR ((status)::text = 'void'::text)))",
            "CHECK ((num_nonnulls(a, b) = 1))",
            "CHECK ((octet_length(file_sha1) = 20))",
            "CHECK ((char_length(name) >= 3))",
            "CHECK ((char_length(a) <= char_length(b)))",
            "CHECK ((a IS NOT NULL) AND (b IS NOT NULL))",
            "CHECK ((starts_with(path, 'x'::text)))",
            "CHECK (((name)::text ~ '^[a-z]+$'::text))",
        ] {
            assert_eq!(interpret(definition), Meaning::Unknown, "{definition}");
        }
    }

    #[test]
    fn a_disjunction_is_not_mistaken_for_its_first_half() {
        // The parenthesis stripper must not turn `(a) OR (b)` into `a) OR (b`
        // and then match on the wreckage.
        assert_eq!(
            interpret("CHECK (((file IS NULL) OR (char_length(file) <= 255)))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn a_zero_or_negative_bound_is_not_a_length_limit() {
        // `char_length(x) <= 0` means the column must be empty, which is a
        // real rule and not one to treat as an ordinary limit.
        assert_eq!(interpret("CHECK ((char_length(name) <= 0))"), Meaning::Unknown);
    }

    #[test]
    fn something_that_is_not_a_check_at_all_is_unknown() {
        assert_eq!(interpret("FOREIGN KEY (a) REFERENCES b(id)"), Meaning::Unknown);
        assert_eq!(interpret(""), Meaning::Unknown);
    }
}
