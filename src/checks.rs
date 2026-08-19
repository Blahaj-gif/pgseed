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
    /// `octet_length(col) = N`. A fixed-width bytea — a hash, usually.
    ByteLength { column: String, exact: i32 },
    /// `(col IS NULL) OR <anything at all>`.
    ///
    /// The one rule here that needs no understanding of the expression it
    /// appears in. A disjunction is satisfied by any one branch, and this
    /// branch is reachable by writing NULL — so whatever the other half says,
    /// however baroque, the constraint holds. It carries an obligation rather
    /// than a permission: the column *must* be left NULL, and a caller that
    /// ignores that has broken the guarantee rather than merely wasted it.
    MustBeNull { column: String },
    /// `col > N` or `col >= N` against a plain integer.
    LowerBound {
        column: String,
        min: i64,
        inclusive: bool,
    },
    /// `col = lower(col)`, however casted. PowerDNS puts this on four of its
    /// seven tables and it refused the entire schema — for a rule satisfied by
    /// generating a lowercase string, which everything here already does.
    Lowercase { column: String },
    /// `num_nonnulls(a, b, ...) = 1` — exactly one of these columns holds a
    /// value and the rest are NULL. GitLab's commonest unrecognised shape, at
    /// 78 of the 277 this did not understand, and it is a *choice* rather than
    /// a fact: one column is filled and the others are obliged to be null.
    ExactlyOneNonNull { columns: Vec<String> },
    /// `jsonb_typeof(col) = 'object'`, and the same for the other five JSON
    /// types. Satisfied by generating a value of that shape, which needs no
    /// understanding of the expression beyond the name of the type wanted.
    JsonType { column: String, kind: String },
    /// `octet_length(col) <= N`. A byte ceiling rather than the exact width
    /// `ByteLength` records — every string this generates is ASCII, so a byte
    /// is a character and the existing length machinery covers it.
    ByteLimit { column: String, max: i32 },
    /// `cardinality(col) <= N`. Every array this generates holds one element,
    /// so any limit of 1 or more is already met.
    CardinalityLimit { column: String, max: i32 },
    /// `col <= N` or `col < N` against a plain literal. A ceiling on a
    /// generated number, and the mirror of `LowerBound` — which existed on its
    /// own for a while, so `(rollout >= 0) AND (rollout <= 10000)` was half
    /// understood and therefore refused.
    UpperBound {
        column: String,
        max: i64,
        inclusive: bool,
    },
    /// `char_length(col) >= N`, or `> N`, or the same with `length`. A floor
    /// rather than a ceiling, and satisfied by padding a value that falls
    /// short. Eight of these across the corpus, which is few, but a schema
    /// somebody writes by hand rather than generates is full of
    /// `char_length(title) > 0`, and that is the reader this refuses to serve
    /// if it does not read them.
    MinLength { column: String, min: i32 },
    /// `col <> ''`. Not "different from some value" — different from the
    /// *empty string* specifically, which every value this generates already
    /// is. The commonest unrecognised shape across eighteen schemas, at 20.
    NonEmpty { column: String },
    /// `(a IS NOT NULL) OR (b IS NOT NULL)` — at least one holds a value.
    /// Weaker than `ExactlyOneNonNull` and satisfied by filling all of them,
    /// which is what happens anyway.
    AtLeastOneNonNull { columns: Vec<String> },
    /// `col = ANY (ARRAY['a', 'b'])`. A value set, which is an enum written
    /// out longhand, and satisfied the same way, by writing one of them.
    /// The literals are kept exactly as Postgres printed them, casts and all,
    /// because that is already valid SQL and re-rendering could only lose
    /// something.
    ValueSet { column: String, values: Vec<String> },
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
        && text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then(|| text.to_string())
}

/// The single argument of a call, if the text is exactly that call.
///
/// `char_length(name)` gives `name`; anything else, including a call with two
/// arguments or a trailing operator, gives nothing.
fn call_argument<'t>(text: &'t str, name: &str) -> Option<&'t str> {
    let inner = unwrap_parens(text)
        .strip_prefix(name)
        .map(str::trim)?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    // No unbalanced parenthesis may remain, or `f(g(x)) + 1` would parse as a
    // call on `g(x)) + 1`.
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

/// A column name with the cast Postgres prints stripped: `(kroki_url)::text`
/// is `kroki_url`. Without this, the same length limit written with a cast
/// went unrecognised — nine of them in the corpus.
fn column_cast(text: &str) -> Option<String> {
    let text = unwrap_parens(text.trim());
    // Only where the cast is the outermost thing. `(data)::jsonb ->> 'x'`
    // starts with one and is an expression; taking the text before the first
    // `::` reads it as the column `data` and drops the rest of the rule on the
    // floor. Found in the index reader, which has the same shape, and fixed
    // here before it could bite.
    let Some((left, right)) = text.split_once("::") else {
        return column_name(text);
    };
    let right = right.trim();
    let is_type_name = !right.is_empty()
        && right
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ');
    is_type_name.then(|| column_name(left.trim())).flatten()
}

/// Whether a fragment is a constant this can write back out verbatim.
///
/// Deliberately narrow: a quoted string, an integer, or a boolean, each with
/// an optional cast. A function call is not one, and neither is a column, so
/// `col = lower(col)` and `col = date_trunc(...)` fall past this to the rules
/// that know what they mean.
fn is_a_literal(text: &str) -> bool {
    let text = unwrap_parens(text.trim()).trim();
    let text = match text.split_once("::") {
        Some((left, right))
            if !right.trim().is_empty()
                && right
                    .trim()
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ') =>
        {
            unwrap_parens(left.trim()).trim()
        }
        _ => text,
    };
    const QUOTE: char = '\u{27}';
    if text.chars().count() >= 2 && text.starts_with(QUOTE) && text.ends_with(QUOTE) {
        // No embedded quote: a doubled one means this is really an escape, and
        // reproducing it is not worth guessing at.
        return !text[1..text.len() - 1].contains(QUOTE);
    }
    matches!(text, "true" | "false") || text.parse::<i64>().is_ok()
}

/// An integer bound, written `255` or `(255)::integer`.
fn integer_bound(text: &str) -> Option<i64> {
    unwrap_parens(unwrap_parens(text).split("::").next()?.trim())
        .trim()
        .parse::<i64>()
        .ok()
}

/// Split on an operator only where it is the top-level one, so a comparison
/// inside a call argument does not look like the comparison being matched.
fn split_top<'t>(text: &'t str, operator: &str) -> Option<(&'t str, &'t str)> {
    let mut depth = 0i32;
    let bytes = text.as_bytes();
    for index in 0..bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && text[index..].starts_with(operator) => {
                return Some((&text[..index], &text[index + operator.len()..]));
            }
            _ => {}
        }
    }
    None
}

/// Split a comma-separated list at the top level, respecting parentheses,
/// brackets and quotes.
fn split_top_all(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start, mut quoted) = (0i32, 0usize, false);
    for (index, ch) in text.char_indices() {
        match ch {
            '\u{27}' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out
}

/// Split on every top-level occurrence of a keyword operator.
///
/// Parentheses and quotes are respected, so the `OR` inside
/// `(a AND (b OR c))` is not a top-level one and the split does not find it.
fn split_keyword<'t>(text: &'t str, operator: &str) -> Vec<&'t str> {
    let mut out = Vec::new();
    let (mut depth, mut start, mut quoted) = (0i32, 0usize, false);
    let mut index = 0usize;
    let bytes = text.as_bytes();
    while index < bytes.len() {
        match bytes[index] {
            0x27 => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            _ if !quoted && depth == 0 && text[index..].starts_with(operator) => {
                out.push(&text[start..index]);
                index += operator.len();
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    out.push(&text[start..]);
    out
}

/// `((a IS NOT NULL) AND (b IS NULL)) OR ((a IS NULL) AND (b IS NOT NULL))`,
/// and the same for any number of columns.
///
/// Returns the columns only when the disjunction is a **complete** cover: the
/// same column set in every branch, exactly one of them non-null in each, and
/// every column filling that role in exactly one branch. That is precisely
/// `num_nonnulls(a, b, ...) = 1` and is satisfied the same way.
///
/// A partial cover — three columns but only two branches — is a narrower
/// obligation that says *these* two may hold the value and the third may not.
/// It is perfectly satisfiable and it is not this, so it returns nothing and
/// its table is refused rather than filled on a guess about which column is
/// allowed to be the filled one.
fn exactly_one_longhand(expression: &str) -> Option<Vec<String>> {
    let branches = split_keyword(expression, " OR ");
    if branches.len() < 2 {
        return None;
    }

    let mut filled: Vec<String> = Vec::new();
    let mut columns: Option<std::collections::BTreeSet<String>> = None;

    for branch in branches {
        let mut here: std::collections::BTreeSet<String> = Default::default();
        let mut here_filled: Option<String> = None;

        for term in split_keyword(unwrap_parens(branch.trim()), " AND ") {
            let text = unwrap_parens(term.trim()).trim();
            let (column, holds_a_value) = if let Some(rest) = text.strip_suffix("IS NOT NULL") {
                (column_cast(rest.trim())?, true)
            } else if let Some(rest) = text.strip_suffix("IS NULL") {
                (column_cast(rest.trim())?, false)
            } else {
                return None;
            };
            // A column named twice in one branch is saying two things about
            // itself and this is not going to work out which.
            if !here.insert(column.clone()) {
                return None;
            }
            if holds_a_value {
                if here_filled.is_some() {
                    return None;
                }
                here_filled = Some(column);
            }
        }

        let here_filled = here_filled?;
        match &columns {
            None => columns = Some(here),
            Some(seen) if *seen == here => {}
            // Different branches over different columns. Whatever that means,
            // it is not this.
            Some(_) => return None,
        }
        if filled.contains(&here_filled) {
            return None;
        }
        filled.push(here_filled);
    }

    let columns = columns?;
    if columns.len() < 2 || filled.len() != columns.len() {
        return None;
    }
    Some(columns.into_iter().collect())
}

/// Everything a constraint requires, as separate obligations.
///
/// A CHECK is usually one rule and `interpret` reads it. Some are several
/// joined by AND, and each half is a rule this already knows:
///
/// ```text
///   (char_length(name) <= 32) AND (char_length(name) >= 3)
///   (jsonb_typeof(min_cursor) = 'array') AND (jsonb_typeof(max_cursor) = 'array')
/// ```
///
/// Both were refused, not because either half was hard but because nothing
/// split them. Eighteen of the sixty-two constraints still refusing a table
/// after the database itself had been asked were this shape.
///
/// A conjunction holds exactly when every part holds, so reading it as its
/// parts is not an approximation. **Every part must be understood** — one
/// `Unknown` makes the whole thing unknown, because satisfying the halves you
/// recognise while ignoring the half you do not is precisely the silent pass
/// this project exists to avoid. OR is not split, and must not be: a
/// disjunction is satisfied by *either* side and choosing which is a decision
/// this has no basis for.
pub fn interpret_all(definition: &str) -> Vec<Meaning> {
    let whole = interpret(definition);
    if whole != Meaning::Unknown {
        return vec![whole];
    }

    let body = definition.trim();
    let Some(rest) = body.strip_prefix("CHECK") else {
        return vec![Meaning::Unknown];
    };
    let rest = rest.trim().trim_end_matches("NOT VALID").trim();
    let parts = split_keyword(unwrap_parens(rest), " AND ");
    if parts.len() < 2 {
        return vec![Meaning::Unknown];
    }

    let read: Vec<Meaning> = parts
        .iter()
        .map(|part| interpret(&format!("CHECK ({})", part.trim())))
        .collect();
    if read.contains(&Meaning::Unknown) {
        return vec![Meaning::Unknown];
    }
    read
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

    if let Some(Some(column)) = expression.strip_suffix("IS NOT NULL").map(column_name) {
        return Meaning::NotNull { column };
    }

    // `col = 'X'` — one permitted value, which is a `ValueSet` with a single
    // member and satisfied by writing it. `lock = 'X'::bpchar` is the
    // single-row-table idiom and appears five times in the corpus.
    if let Some((left, right)) = split_top(expression, "=") {
        if !left.ends_with(['<', '>', '!']) && !right.starts_with('=') {
            if let Some(column) = column_cast(left) {
                let literal = right.trim();
                if is_a_literal(literal) {
                    return Meaning::ValueSet {
                        column,
                        values: vec![literal.to_string()],
                    };
                }
            }
        }
    }

    // Exactly one of them holds a value, written out longhand:
    //
    //   ((a IS NOT NULL) AND (b IS NULL)) OR ((a IS NULL) AND (b IS NOT NULL))
    //
    // A third spelling of `num_nonnulls(a, b) = 1`, and the one an application
    // written by hand tends to reach for. Placed before the `(col IS NULL) OR`
    // rule below, because a disjunct beginning `(a IS NULL)` would otherwise
    // be read as licence to null that column out and nothing else.
    if let Some(columns) = exactly_one_longhand(expression) {
        return Meaning::ExactlyOneNonNull { columns };
    }

    // `(col IS NULL) OR ...` — satisfied by writing NULL, whatever follows.
    // Matched only as the *leading* branch of a top-level disjunction, so that
    // `a = 1 OR (b IS NULL)` cannot be read as licence to null out `b`.
    if let Some(rest) = expression.strip_prefix('(') {
        if let Some((first, tail)) = rest.split_once(')') {
            if tail.trim_start().starts_with("OR ") {
                if let Some(Some(column)) = first
                    .strip_suffix("IS NULL")
                    .map(str::trim)
                    .map(column_name)
                {
                    return Meaning::MustBeNull { column };
                }
            }
        }
    }

    // `col = lower(col)`, with the casts Postgres prints. Both sides must name
    // the same column, or this is a comparison between two things and not a
    // statement about one.
    if let Some((left, right)) = expression.split_once('=') {
        if !left.ends_with('<') && !left.ends_with('>') && !left.ends_with('!') {
            let subject = column_name(unwrap_parens(left).split("::").next().unwrap_or(""));
            let called = unwrap_parens(right)
                .strip_prefix("lower")
                .map(str::trim)
                .and_then(|s| s.strip_prefix('('))
                .and_then(|s| s.strip_suffix(')'))
                .map(|inner| column_name(inner.split("::").next().unwrap_or("")));
            if let (Some(subject), Some(Some(inner))) = (subject, called) {
                if subject == inner {
                    return Meaning::Lowercase { column: subject };
                }
            }
        }
    }

    // `octet_length(col) = N` — a fixed-width bytea, almost always a hash.
    if let Some((left, right)) = expression.split_once('=') {
        if !left.ends_with('<') && !left.ends_with('>') && !left.ends_with('!') {
            if let Some(inner) = unwrap_parens(left)
                .strip_prefix("octet_length")
                .map(str::trim)
                .and_then(|s| s.strip_prefix('('))
                .and_then(|s| s.strip_suffix(')'))
            {
                if let Some(column) = column_name(inner) {
                    let bound = unwrap_parens(right)
                        .split("::")
                        .next()
                        .map(|s| unwrap_parens(s).trim().to_string());
                    if let Some(exact) = bound.and_then(|b| b.parse::<i32>().ok()) {
                        if exact >= 0 {
                            return Meaning::ByteLength { column, exact };
                        }
                    }
                }
            }
        }
    }

    // `col > N` and `col >= N` against a plain integer literal.
    for (operator, inclusive) in [(">=", true), (">", false)] {
        let Some((left, right)) = expression.split_once(operator) else {
            continue;
        };
        if operator == ">" && right.starts_with('=') {
            continue; // that was `>=`, handled above
        }
        let Some(column) = column_name(left) else {
            continue;
        };
        let bound = unwrap_parens(right)
            .split("::")
            .next()
            .map(|s| unwrap_parens(s).trim().to_string());
        if let Some(min) = bound.and_then(|b| b.parse::<i64>().ok()) {
            return Meaning::LowerBound {
                column,
                min,
                inclusive,
            };
        }
    }

    // `f(col) <= N` for the three measuring functions. The bound is written
    // `255` or `(255)::integer` depending on how it was declared; anything
    // that is not a plain integer once the cast comes off is not a bound this
    // understands.
    if let Some((left, right)) = split_top(expression, "<=") {
        // Written the other way round: `1 <= "position"` is a floor on the
        // column, not a ceiling. GitLab writes bounded ranges this way and the
        // early return below meant the whole conjunction went unread over it.
        if let (Some(min), Some(column)) = (integer_bound(left), column_name(right.trim())) {
            return Meaning::LowerBound {
                column,
                min,
                inclusive: true,
            };
        }
        let Some(max) = integer_bound(right).and_then(|n| i32::try_from(n).ok()) else {
            return Meaning::Unknown;
        };
        for call in ["char_length", "length"] {
            if let Some(column) = call_argument(left, call).and_then(column_cast) {
                if max > 0 {
                    return Meaning::LengthLimit { column, max };
                }
            }
        }
        // A byte ceiling. Every string this generates is ASCII, so a byte is a
        // character and the same limit does the job — which is what makes this
        // provable rather than approximately right.
        if let Some(column) = call_argument(left, "octet_length").and_then(column_cast) {
            if max > 0 {
                return Meaning::ByteLimit { column, max };
            }
        }
        // Every array this generates holds exactly one element.
        if let Some(column) = call_argument(left, "cardinality").and_then(column_cast) {
            if max >= 1 {
                return Meaning::CardinalityLimit { column, max };
            }
        }
        // A plain ceiling on the column itself, which is what is left once the
        // measuring functions have had their turn.
        if let Some(column) = column_name(left.trim()) {
            if let Some(max) = integer_bound(right) {
                return Meaning::UpperBound {
                    column,
                    max,
                    inclusive: true,
                };
            }
        }
        return Meaning::Unknown;
    }

    // `col < N`, and `char_length(col) < N` — a strict ceiling, which is the
    // inclusive one at N - 1. `<=` is tried first above, so anything reaching
    // here really is the one-character operator.
    if let Some((left, right)) = split_top(expression, "<") {
        if !right.starts_with('=') && !right.starts_with('>') {
            // `0 < col` is a floor of one, the reversed twin of `col > 0`.
            if let (Some(min), Some(column)) = (integer_bound(left), column_name(right.trim())) {
                return Meaning::LowerBound {
                    column,
                    min: min + 1,
                    inclusive: true,
                };
            }
            if let Some(bound) = integer_bound(right) {
                for call in ["char_length", "length"] {
                    if let Some(column) = call_argument(left, call).and_then(column_cast) {
                        if let Ok(max) = i32::try_from(bound - 1) {
                            if max > 0 {
                                return Meaning::LengthLimit { column, max };
                            }
                        }
                        return Meaning::Unknown;
                    }
                }
                if let Some(column) = column_name(left.trim()) {
                    return Meaning::UpperBound {
                        column,
                        max: bound - 1,
                        inclusive: true,
                    };
                }
            }
        }
    }

    // The same three measuring functions the other way up. `>= N` is a floor
    // of N and `> N` a floor of N + 1, and a floor is satisfiable by padding
    // where a ceiling is satisfiable by truncating.
    //
    // Bounded above at 4096 because the point of reading it is to satisfy it,
    // and a column obliged to hold a megabyte is one this would rather refuse
    // than fill.
    for (operator, offset) in [(">=", 0i64), (">", 1)] {
        let Some((left, right)) = split_top(expression, operator) else {
            continue;
        };
        // `>=` also splits on `>`, so the two-character form has to be tried
        // first and the one-character form must not match its leftovers.
        if operator == ">" && (left.ends_with(['<', '>', '!']) || right.starts_with('=')) {
            continue;
        }
        let Some(bound) = integer_bound(right) else {
            continue;
        };
        for call in ["char_length", "length"] {
            if let Some(column) = call_argument(left, call).and_then(column_cast) {
                let min = bound + offset;
                if (0..=4096).contains(&min) {
                    return Meaning::MinLength {
                        column,
                        min: min as i32,
                    };
                }
                return Meaning::Unknown;
            }
        }
    }

    // `num_nonnulls(a, b, ...) = N` where N is how many arguments there are:
    // every one of them holds a value, which is what happens anyway. And
    // `>= 1` or `> 0`, which is at-least-one and satisfied the same way.
    for (operator, wanted) in [("=", None), (">=", Some(1i64)), (">", Some(0))] {
        let Some((left, right)) = split_top(expression, operator) else {
            continue;
        };
        if operator == "=" && (left.ends_with(['<', '>', '!']) || right.starts_with('=')) {
            continue;
        }
        if operator == ">" && (right.starts_with('=') || left.ends_with(['<', '>', '!'])) {
            continue;
        }
        let Some(arguments) = call_argument(left, "num_nonnulls") else {
            continue;
        };
        let columns: Vec<Option<String>> = split_top_all(arguments)
            .iter()
            .map(|a| column_cast(a))
            .collect();
        if columns.len() < 2 || !columns.iter().all(Option::is_some) {
            continue;
        }
        let columns: Vec<String> = columns.into_iter().flatten().collect();
        let bound = integer_bound(right);
        let matches = match wanted {
            // `= N` only when N is every argument.
            None => bound == Some(columns.len() as i64),
            Some(n) => bound == Some(n),
        };
        if matches {
            return Meaning::AtLeastOneNonNull { columns };
        }
    }

    // `num_nonnulls(a, b, ...) = 1` — exactly one of them holds a value.
    //
    // Only `= 1`. `<= 1` is satisfied by nulling all of them and `> 0` by
    // filling all of them, and both are perfectly satisfiable, but they are
    // different obligations and each needs its own shape rather than being
    // folded in here on the grounds of looking similar.
    if let Some((left, right)) = split_top(expression, "=") {
        if !left.ends_with(['<', '>', '!']) && integer_bound(right) == Some(1) {
            if let Some(arguments) = call_argument(left, "num_nonnulls") {
                let columns: Vec<Option<String>> = arguments.split(',').map(column_cast).collect();
                if columns.len() >= 2 && columns.iter().all(Option::is_some) {
                    return Meaning::ExactlyOneNonNull {
                        columns: columns.into_iter().flatten().collect(),
                    };
                }
            }
        }
    }

    // `col <> ''` — a column that must not be the empty string. Every value
    // this generates is at least one character, so it holds by construction.
    // Only against the empty string: `col <> 'draft'` is a different rule
    // that happens to look the same, and nothing here can satisfy it.
    if let Some((left, right)) = split_top(expression, "<>") {
        if let Some(column) = column_cast(left) {
            let literal = unwrap_parens(right).trim();
            let literal = literal.split("::").next().unwrap_or("").trim();
            if literal == "''" {
                return Meaning::NonEmpty { column };
            }
        }
    }

    // Two more spellings of exactly-one-non-null. `(a IS NULL) <> (b IS NULL)`
    // says the two differ in their nullness, which for two columns is the same
    // obligation `num_nonnulls(a, b) = 1` states. GitLab writes it both ways
    // and Lago prefers this one; 15 across the corpus.
    if let Some((left, right)) = split_top(expression, "<>") {
        for suffix in ["IS NULL", "IS NOT NULL"] {
            let both = [left, right].map(|side| {
                unwrap_parens(side.trim())
                    .strip_suffix(suffix)
                    .map(str::trim)
                    .and_then(column_cast)
            });
            // `IS NOT NULL` also ends with `IS NULL` read naively, so the
            // NOT form has to be ruled out before the plain one matches.
            let plain_form_is_really_the_not_form = suffix == "IS NULL"
                && [left, right]
                    .iter()
                    .any(|side| unwrap_parens(side.trim()).ends_with("IS NOT NULL"));
            if let [Some(a), Some(b)] = both {
                if a != b && !plain_form_is_really_the_not_form {
                    return Meaning::ExactlyOneNonNull {
                        columns: vec![a, b],
                    };
                }
            }
        }
    }

    // `(a IS NOT NULL) OR (b IS NOT NULL)` — at least one of them holds a
    // value, which filling all of them satisfies.
    if expression.contains(" OR ") {
        let parts: Vec<&str> = expression.split(" OR ").collect();
        let columns: Vec<Option<String>> = parts
            .iter()
            .map(|part| {
                unwrap_parens(part.trim())
                    .strip_suffix("IS NOT NULL")
                    .map(str::trim)
                    .and_then(column_cast)
            })
            .collect();
        if columns.len() >= 2 && columns.iter().all(Option::is_some) {
            return Meaning::AtLeastOneNonNull {
                columns: columns.into_iter().flatten().collect(),
            };
        }
    }

    // `col = ANY (ARRAY[...])` — one of a listed set of values.
    //
    // Only in that exact shape. `NOT ('' = ANY (col))` is a different rule
    // about an array column, and `col = ANY (subquery)` names values this
    // cannot see; both stay unknown.
    if let Some((left, right)) = split_top(expression, "=") {
        if !left.ends_with(['<', '>', '!']) {
            if let Some(column) = column_cast(left) {
                if let Some(list) = call_argument(unwrap_parens(right.trim()), "ANY")
                    .and_then(|inner| call_argument(inner, "ARRAY"))
                    .or_else(|| {
                        // `ANY (ARRAY[...])` prints with brackets rather than
                        // parentheses around the elements.
                        call_argument(unwrap_parens(right.trim()), "ANY").and_then(|inner| {
                            unwrap_parens(inner.trim())
                                .strip_prefix("ARRAY[")
                                .and_then(|s| s.strip_suffix(']'))
                        })
                    })
                {
                    let values: Vec<String> = split_top_all(list)
                        .into_iter()
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .collect();
                    if !values.is_empty() {
                        return Meaning::ValueSet { column, values };
                    }
                }
            }
        }
    }

    // `jsonb_typeof(col) = 'object'`, and the five other JSON types.
    if let Some((left, right)) = split_top(expression, "=") {
        if !left.ends_with(['<', '>', '!']) {
            for call in ["jsonb_typeof", "json_typeof"] {
                let Some(column) = call_argument(left, call).and_then(column_cast) else {
                    continue;
                };
                let literal = unwrap_parens(right).trim();
                let literal = literal.split("::").next().unwrap_or("").trim();
                let Some(kind) = literal
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                else {
                    continue;
                };
                if ["object", "array", "string", "number", "boolean", "null"].contains(&kind) {
                    return Meaning::JsonType {
                        column,
                        kind: kind.to_string(),
                    };
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
            Meaning::LengthLimit {
                column: "name".into(),
                max: 255
            }
        );
        assert_eq!(
            interpret("CHECK ((length(title) <= 64))"),
            Meaning::LengthLimit {
                column: "title".into(),
                max: 64
            }
        );
    }

    #[test]
    fn a_bound_written_as_a_cast_is_still_a_bound() {
        assert_eq!(
            interpret("CHECK ((char_length(path) <= (255)::integer))"),
            Meaning::LengthLimit {
                column: "path".into(),
                max: 255
            }
        );
    }

    #[test]
    fn a_not_null_written_as_a_check_is_recognised() {
        // 414 of GitLab's constraints are this, usually left behind by a
        // migration that adds NOT NULL without a table rewrite.
        assert_eq!(
            interpret("CHECK ((description IS NOT NULL))"),
            Meaning::NotNull {
                column: "description".into()
            }
        );
    }

    #[test]
    fn not_valid_does_not_change_what_a_constraint_requires() {
        // NOT VALID means existing rows were not checked. New ones still are,
        // and every row this writes is a new one.
        assert_eq!(
            interpret("CHECK ((char_length(name) <= 255)) NOT VALID"),
            Meaning::LengthLimit {
                column: "name".into(),
                max: 255
            }
        );
    }

    #[test]
    fn a_quoted_column_keeps_its_real_name() {
        assert_eq!(
            interpret("CHECK ((char_length(\"user name\") <= 40))"),
            Meaning::LengthLimit {
                column: "user name".into(),
                max: 40
            }
        );
    }

    #[test]
    fn anything_outside_the_closed_set_is_unknown() {
        // The whole safety of this module. Each of these is refused, and none
        // of them is approximated.
        for definition in [
            "CHECK (((total > 0) OR ((status)::text = 'void'::text)))",
            "CHECK ((char_length(a) <= char_length(b)))",
            // A floor is read; a floor against another column is not.
            "CHECK ((char_length(a) >= char_length(b)))",
            "CHECK ((a IS NOT NULL) AND (b IS NOT NULL))",
            "CHECK ((starts_with(path, 'x'::text)))",
            "CHECK (((name)::text ~ '^[a-z]+$'::text))",
            // The near misses of the shapes that *are* understood. Each is
            // perfectly satisfiable and each is a different obligation, so
            // each needs its own shape rather than being folded into a
            // neighbour on the grounds of looking similar.
            "CHECK ((num_nonnulls(a, b) <= 1))",
            "CHECK ((num_nonnulls(a, b, c) = 2))",
            "CHECK ((num_nonnulls(a) = 1))",
            "CHECK (((num_nonnulls(a, b) = 1) OR (num_nulls(a, b) = 2)))",
            "CHECK ((num_nonnulls(a, lower(b)) = 1))",
            "CHECK ((jsonb_typeof(payload) = 'wibble'::text))",
            "CHECK ((jsonb_typeof(payload) <> 'object'::text))",
            "CHECK ((cardinality(tags) <= 0))",
            "CHECK ((octet_length(a) <= octet_length(b)))",
            "CHECK ((char_length(name) <= 255) AND (char_length(name) >= 3))",
        ] {
            assert_eq!(interpret(definition), Meaning::Unknown, "{definition}");
        }
    }

    #[test]
    fn a_value_set_is_read_and_a_negated_one_is_not() {
        assert_eq!(
            interpret("CHECK ((status = ANY (ARRAY['created'::text, 'error'::text])))"),
            Meaning::ValueSet {
                column: "status".into(),
                values: vec!["'created'::text".into(), "'error'::text".into()],
            }
        );
        assert_eq!(
            interpret("CHECK ((cadence = ANY (ARRAY[1, 7, 14, 30, 90])))"),
            Meaning::ValueSet {
                column: "cadence".into(),
                values: ["1", "7", "14", "30", "90"]
                    .iter()
                    .map(|v| v.to_string())
                    .collect(),
            }
        );
        // A rule about what the column may *not* be, and one about the
        // contents of an array column. Neither is this shape.
        assert_eq!(
            interpret("CHECK ((NOT (''::text = ANY ((permissions)::text[]))))"),
            Meaning::Unknown
        );
        assert_eq!(
            interpret("CHECK ((status <> ANY (ARRAY['a'::text])))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn at_least_one_and_all_of_them_are_read_as_the_obligations_they_are() {
        // `> 0` and `>= 1` are at-least-one; `= N` over N arguments is all of
        // them. Each is satisfied by filling every column, which is what
        // happens anyway — so each has its own meaning rather than being
        // folded into `= 1`, which is a different rule entirely.
        for definition in [
            "CHECK ((num_nonnulls(a, b) > 0))",
            "CHECK ((num_nonnulls(a, b) >= 1))",
            "CHECK ((num_nonnulls(a, b) = 2))",
        ] {
            assert_eq!(
                interpret(definition),
                Meaning::AtLeastOneNonNull {
                    columns: vec!["a".into(), "b".into()]
                },
                "{definition}"
            );
        }
        // `= 2` over three arguments is two of the three, which is neither.
        assert_eq!(
            interpret("CHECK ((num_nonnulls(a, b, c) = 2))"),
            Meaning::Unknown
        );
        // And nulling them all still satisfies `<= 1`, which is a different
        // obligation and stays out.
        assert_eq!(
            interpret("CHECK ((num_nonnulls(a, b) <= 1))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn a_conjunction_is_read_as_its_parts_only_when_every_part_is_understood() {
        // Both halves known: two obligations, both recorded.
        let both =
            interpret_all("CHECK (((char_length(name) <= 32) AND (char_length(name) >= 3)))");
        assert_eq!(
            both,
            vec![
                Meaning::LengthLimit {
                    column: "name".into(),
                    max: 32
                },
                Meaning::MinLength {
                    column: "name".into(),
                    min: 3
                }
            ]
        );
        // One half unknown makes the whole thing unknown. Satisfying the half
        // that is recognised and ignoring the other is the silent pass this
        // whole project is built against.
        assert_eq!(
            interpret_all("CHECK (((char_length(name) <= 32) AND (name ~ '^[a-z]+$'::text)))"),
            vec![Meaning::Unknown]
        );
        // OR is never split: a disjunction is satisfied by either side and
        // there is no basis here for choosing which.
        assert_eq!(
            interpret_all("CHECK (((char_length(a) <= 4) OR (char_length(b) <= 4)))"),
            vec![Meaning::Unknown]
        );
    }

    #[test]
    fn a_single_permitted_value_is_a_value_set_of_one() {
        assert_eq!(
            interpret("CHECK ((lock = 'X'::bpchar))"),
            Meaning::ValueSet {
                column: "lock".into(),
                values: vec!["'X'::bpchar".into()]
            }
        );
        assert_eq!(
            interpret("CHECK ((notify_owner = false))"),
            Meaning::ValueSet {
                column: "notify_owner".into(),
                values: vec!["false".into()]
            }
        );
        // A column compared with an expression is not a permitted value.
        assert_eq!(
            interpret("CHECK ((date = date_trunc('month'::text, date)))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn a_ceiling_on_a_number_is_read_the_same_way_as_a_floor() {
        assert_eq!(
            interpret("CHECK ((rollout <= 10000))"),
            Meaning::UpperBound {
                column: "rollout".into(),
                max: 10000,
                inclusive: true
            }
        );
        // A strict ceiling is the inclusive one a step down, for the column
        // and for its length alike.
        assert_eq!(
            interpret("CHECK ((attempt < 20))"),
            Meaning::UpperBound {
                column: "attempt".into(),
                max: 19,
                inclusive: true
            }
        );
        assert_eq!(
            interpret("CHECK ((char_length(queue) < 128))"),
            Meaning::LengthLimit {
                column: "queue".into(),
                max: 127
            }
        );
        // Against another column it is a comparison, not a bound.
        assert_eq!(
            interpret("CHECK ((attempt <= max_attempts))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn exactly_one_of_them_is_recognised_written_out_longhand() {
        // A third spelling of `num_nonnulls(a, b) = 1`. Plausible writes it
        // this way and so does GitLab, over three columns.
        assert_eq!(
            interpret(
                "CHECK ((((event_name IS NOT NULL) AND (page_path IS NULL)) OR \
                 ((event_name IS NULL) AND (page_path IS NOT NULL))))"
            ),
            Meaning::ExactlyOneNonNull {
                columns: vec!["event_name".into(), "page_path".into()]
            }
        );
        assert_eq!(
            interpret(
                "CHECK ((((access_level IS NOT NULL) AND (group_id IS NULL) AND (user_id IS NULL)) \
                 OR ((user_id IS NOT NULL) AND (access_level IS NULL) AND (group_id IS NULL)) OR \
                 ((group_id IS NOT NULL) AND (user_id IS NULL) AND (access_level IS NULL))))"
            ),
            Meaning::ExactlyOneNonNull {
                columns: vec!["access_level".into(), "group_id".into(), "user_id".into()]
            }
        );
    }

    #[test]
    fn a_longhand_disjunction_that_is_not_a_complete_cover_is_refused() {
        for definition in [
            // Three columns, two branches: this says `c` may never be the one
            // holding the value, which is a narrower rule and not this one.
            "CHECK ((((a IS NOT NULL) AND (b IS NULL) AND (c IS NULL)) OR \
             ((b IS NOT NULL) AND (a IS NULL) AND (c IS NULL))))",
            // Branches over different column sets.
            "CHECK ((((a IS NOT NULL) AND (b IS NULL)) OR ((c IS NULL) AND (d IS NOT NULL))))",
            // Two non-nulls in one branch is at-least-two, not exactly-one.
            "CHECK ((((a IS NOT NULL) AND (b IS NOT NULL)) OR ((a IS NULL) AND (b IS NULL))))",
            // The same column filling the role twice.
            "CHECK ((((a IS NOT NULL) AND (b IS NULL)) OR ((a IS NOT NULL) AND (b IS NULL))))",
            // Something that is not a null test at all.
            "CHECK ((((a IS NOT NULL) AND (b = 1)) OR ((a IS NULL) AND (b IS NOT NULL))))",
        ] {
            assert_eq!(interpret(definition), Meaning::Unknown, "{definition}");
        }
    }

    #[test]
    fn a_floor_on_the_length_is_read_the_same_way_as_a_ceiling() {
        assert_eq!(
            interpret("CHECK ((char_length(title) > 0))"),
            Meaning::MinLength {
                column: "title".into(),
                min: 1
            }
        );
        // `>= 3` is a floor of three; `> 3` is a floor of four. Reading them
        // as the same number is off by one on every row.
        assert_eq!(
            interpret("CHECK ((char_length(name) >= 3))"),
            Meaning::MinLength {
                column: "name".into(),
                min: 3
            }
        );
        assert_eq!(
            interpret("CHECK ((length(code) > 3))"),
            Meaning::MinLength {
                column: "code".into(),
                min: 4
            }
        );
        assert_eq!(
            interpret("CHECK ((char_length((slug)::text) >= 2))"),
            Meaning::MinLength {
                column: "slug".into(),
                min: 2
            }
        );
        // A floor nobody could want filled is refused rather than honoured
        // with four kilobytes of padding.
        assert_eq!(
            interpret("CHECK ((char_length(blob) >= 100000))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn not_the_empty_string_is_understood_and_not_the_other_value_is_not() {
        assert_eq!(
            interpret("CHECK ((name <> ''::text))"),
            Meaning::NonEmpty {
                column: "name".into()
            }
        );
        assert_eq!(
            interpret("CHECK (((name)::text <> ''::text))"),
            Meaning::NonEmpty {
                column: "name".into()
            }
        );
        // A different value is a different rule that happens to look alike.
        assert_eq!(
            interpret("CHECK ((status <> 'draft'::text))"),
            Meaning::Unknown
        );
        assert_eq!(interpret("CHECK ((a <> b))"), Meaning::Unknown);
    }

    #[test]
    fn the_two_other_spellings_of_exactly_one_are_read_as_the_same_rule() {
        // GitLab writes num_nonnulls; Lago prefers this. Same obligation.
        assert_eq!(
            interpret("CHECK (((project_id IS NULL) <> (namespace_id IS NULL)))"),
            Meaning::ExactlyOneNonNull {
                columns: vec!["project_id".into(), "namespace_id".into()]
            }
        );
        assert_eq!(
            interpret("CHECK (((plan_id IS NOT NULL) <> (subscription_id IS NOT NULL)))"),
            Meaning::ExactlyOneNonNull {
                columns: vec!["plan_id".into(), "subscription_id".into()]
            }
        );
        // The same column on both sides says nothing, and must not be read as
        // an obligation to null one of them.
        assert_eq!(
            interpret("CHECK (((a IS NULL) <> (a IS NULL)))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn at_least_one_is_weaker_than_exactly_one_and_kept_separate() {
        assert_eq!(
            interpret("CHECK (((a IS NOT NULL) OR (b IS NOT NULL)))"),
            Meaning::AtLeastOneNonNull {
                columns: vec!["a".into(), "b".into()]
            }
        );
        assert_eq!(
            interpret("CHECK (((a IS NOT NULL) OR (b IS NOT NULL) OR (c IS NOT NULL)))"),
            Meaning::AtLeastOneNonNull {
                columns: vec!["a".into(), "b".into(), "c".into()]
            }
        );
        // A disjunction with anything else in it is not this shape. The first
        // branch being `IS NULL` is a *nullable escape* and is read as one.
        assert_eq!(
            interpret("CHECK (((a IS NOT NULL) OR (b > 0)))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn exactly_one_of_a_group_is_recognised_and_keeps_the_declared_order() {
        assert_eq!(
            interpret("CHECK ((num_nonnulls(group_id, project_id) = 1))"),
            Meaning::ExactlyOneNonNull {
                columns: vec!["group_id".into(), "project_id".into()]
            }
        );
        assert_eq!(
            interpret("CHECK ((num_nonnulls(namespace_id, organization_id, project_id) = 1))"),
            Meaning::ExactlyOneNonNull {
                columns: vec![
                    "namespace_id".into(),
                    "organization_id".into(),
                    "project_id".into(),
                ]
            }
        );
    }

    #[test]
    fn a_json_type_is_recognised_for_the_six_types_and_nothing_else() {
        for kind in ["object", "array", "string", "number", "boolean", "null"] {
            assert_eq!(
                interpret(&format!("CHECK ((jsonb_typeof(filter) = '{kind}'::text))")),
                Meaning::JsonType {
                    column: "filter".into(),
                    kind: kind.into()
                },
                "{kind}"
            );
        }
    }

    #[test]
    fn a_byte_ceiling_and_an_exact_byte_width_are_different_things() {
        assert_eq!(
            interpret("CHECK ((octet_length(iv) <= 12))"),
            Meaning::ByteLimit {
                column: "iv".into(),
                max: 12
            }
        );
        assert_eq!(
            interpret("CHECK ((octet_length(sha) = 32))"),
            Meaning::ByteLength {
                column: "sha".into(),
                exact: 32
            }
        );
        // NOT VALID says nothing about the rows this is about to write.
        assert_eq!(
            interpret("CHECK ((octet_length(target_sha) <= 64)) NOT VALID"),
            Meaning::ByteLimit {
                column: "target_sha".into(),
                max: 64
            }
        );
    }

    #[test]
    fn an_expression_that_merely_starts_with_a_cast_is_not_a_column() {
        // The bug this is here for: reading `(data)::jsonb ->> 'x'` as the
        // column `data` would accept a length limit on a value that is not
        // that column, and in the index reader it dropped a rule that really
        // does reject rows.
        assert_eq!(
            interpret("CHECK ((char_length(((data)::jsonb ->> 'x'::text)) <= 20))"),
            Meaning::Unknown
        );
        assert_eq!(
            interpret("CHECK ((num_nonnulls(((a)::jsonb ->> 'x'::text), b) = 1))"),
            Meaning::Unknown
        );
        // And the ordinary cast still reads as the column it is.
        assert_eq!(
            interpret("CHECK ((char_length((name)::text) <= 20))"),
            Meaning::LengthLimit {
                column: "name".into(),
                max: 20
            }
        );
    }

    #[test]
    fn a_length_limit_survives_the_cast_postgres_prints() {
        // Nine constraints in the corpus were this shape and were refused for
        // the parentheses alone.
        assert_eq!(
            interpret("CHECK ((char_length((kroki_url)::text) <= 1024))"),
            Meaning::LengthLimit {
                column: "kroki_url".into(),
                max: 1024
            }
        );
    }

    #[test]
    fn an_array_length_limit_is_met_by_the_one_element_this_writes() {
        assert_eq!(
            interpret("CHECK ((cardinality(links_to_spam) <= 20))"),
            Meaning::CardinalityLimit {
                column: "links_to_spam".into(),
                max: 20
            }
        );
    }

    #[test]
    fn a_disjunction_is_not_mistaken_for_its_first_half() {
        // The parenthesis stripper must not turn `(a) OR (b)` into `a) OR (b`
        // and then match on the wreckage. This asserted `Unknown` when the
        // closed set held two shapes; it is now a *nullable escape*, which is
        // a different and stronger answer — writing NULL satisfies the whole
        // disjunction — and the length limit in the second branch is still
        // never evaluated.
        assert_eq!(
            interpret("CHECK (((file IS NULL) OR (char_length(file) <= 255)))"),
            Meaning::MustBeNull {
                column: "file".into()
            }
        );
        // The wreckage case itself, with nothing matchable in either branch.
        assert_eq!(interpret("CHECK (((a = 1) OR (b = 2)))"), Meaning::Unknown);
    }

    #[test]
    fn a_zero_or_negative_bound_is_not_a_length_limit() {
        // `char_length(x) <= 0` means the column must be empty, which is a
        // real rule and not one to treat as an ordinary limit.
        assert_eq!(
            interpret("CHECK ((char_length(name) <= 0))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn a_fixed_byte_length_is_recognised() {
        // 85 of GitLab's constraints are this: a hash column pinned to width.
        assert_eq!(
            interpret("CHECK ((octet_length(file_sha1) = 20))"),
            Meaning::ByteLength {
                column: "file_sha1".into(),
                exact: 20
            }
        );
    }

    #[test]
    fn a_nullable_escape_is_taken_without_reading_the_other_branch() {
        // The nicest rule here: a disjunction holds if ANY branch holds, and
        // writing NULL reaches this one — so the other half can be arbitrarily
        // baroque and it does not matter.
        assert_eq!(
            interpret("CHECK (((file_md5 IS NULL) OR (octet_length(file_md5) = 16)))"),
            Meaning::MustBeNull {
                column: "file_md5".into()
            }
        );
        assert_eq!(
            interpret("CHECK (((x IS NULL) OR (some_baroque_thing(x, y) ~ '^[a-z]+$')))"),
            Meaning::MustBeNull { column: "x".into() }
        );
    }

    #[test]
    fn a_null_branch_that_is_not_the_leading_one_is_not_a_licence() {
        // `a = 1 OR (b IS NULL)` does NOT mean b may be nulled: satisfying it
        // that way requires knowing the first branch is false, which is
        // exactly the reasoning this module refuses to do.
        assert_eq!(
            interpret("CHECK (((a = 1) OR (b IS NULL)))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn a_conjunction_containing_is_null_is_not_a_disjunction() {
        // `(a IS NULL) AND (b > 0)` still requires b > 0. Reading the AND as an
        // OR would write NULL and leave the second half violated.
        assert_eq!(
            interpret("CHECK (((a IS NULL) AND (b > 0)))"),
            Meaning::Unknown
        );
    }

    #[test]
    fn a_lower_bound_on_a_plain_integer_is_recognised() {
        assert_eq!(
            interpret("CHECK ((size >= 0))"),
            Meaning::LowerBound {
                column: "size".into(),
                min: 0,
                inclusive: true
            }
        );
        assert_eq!(
            interpret("CHECK ((count > 0))"),
            Meaning::LowerBound {
                column: "count".into(),
                min: 0,
                inclusive: false
            }
        );
    }

    #[test]
    fn a_bound_against_anything_but_a_literal_is_unknown() {
        // `a > b` compares two columns, and `a > now()` is not a number at all.
        assert_eq!(interpret("CHECK ((a > b))"), Meaning::Unknown);
        assert_eq!(interpret("CHECK ((created_at > now()))"), Meaning::Unknown);
        assert_eq!(
            interpret("CHECK ((total > (0)::numeric))"),
            Meaning::LowerBound {
                column: "total".into(),
                min: 0,
                inclusive: false
            }
        );
    }

    #[test]
    fn a_lowercase_constraint_is_recognised() {
        // PowerDNS, on four of its seven tables, in the form Postgres prints.
        assert_eq!(
            interpret("CHECK (((name)::text = lower((name)::text)))"),
            Meaning::Lowercase {
                column: "name".into()
            }
        );
    }

    #[test]
    fn lowercase_of_a_different_column_is_not_a_statement_about_this_one() {
        // `a = lower(b)` relates two columns. Satisfying it needs both, which
        // is reasoning this does not do.
        assert_eq!(interpret("CHECK ((a = lower(b)))"), Meaning::Unknown);
    }

    #[test]
    fn something_that_is_not_a_check_at_all_is_unknown() {
        assert_eq!(
            interpret("FOREIGN KEY (a) REFERENCES b(id)"),
            Meaning::Unknown
        );
        assert_eq!(interpret(""), Meaning::Unknown);
    }
}
