//! Choosing which tables to fill, and how many rows each one gets.
//!
//! Both are patterns rather than lists, because a schema with nine hundred
//! tables is exactly the one where naming them individually is not an option.
//! The pattern language is two characters — `*` and `?` — deliberately: a
//! regex here would be a second thing to learn and a second thing to get
//! subtly wrong, and nobody filtering table names has ever needed a
//! backreference.

use std::collections::BTreeMap;

use crate::schema::TableId;

/// Match a name against a pattern with `*` (any run, including none) and `?`
/// (exactly one character).
///
/// Iterative with one backtrack point rather than recursive: `*` against a
/// long name is where a naive recursive matcher goes exponential, and a
/// pathological pattern should not be able to hang the tool.
pub fn matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut retry) = (None, 0usize);

    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                retry = n;
                p += 1;
            }
            Some('?') => {
                p += 1;
                n += 1;
            }
            Some(c) if *c == name[n] => {
                p += 1;
                n += 1;
            }
            // No match here. If a `*` came earlier it can swallow one more
            // character and everything after it is tried again.
            _ => match star {
                Some(at) => {
                    p = at + 1;
                    retry += 1;
                    n = retry;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Which tables to touch, from repeatable `--include` and `--exclude` globs.
///
/// A pattern is matched against both the bare name and the qualified one, so
/// `--include orders` works in the ordinary case and `--include billing.*`
/// works when it matters which schema.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Selection {
    /// Include wins on silence and exclude wins on conflict: nothing stated
    /// means everything, and a table named by both is left alone. The
    /// destructive reading of an ambiguous instruction is not the one to take.
    pub fn allows(&self, id: &TableId) -> bool {
        let qualified = format!("{}.{}", id.schema, id.name);
        let hit = |patterns: &[String]| {
            patterns
                .iter()
                .any(|p| matches(p, &id.name) || matches(p, &qualified))
        };
        if hit(&self.exclude) {
            return false;
        }
        self.include.is_empty() || hit(&self.include)
    }
}

/// How many rows each table gets: one default, and any number of overrides.
#[derive(Debug, Clone)]
pub struct RowCounts {
    pub default: usize,
    overrides: Vec<(String, usize)>,
}

impl Default for RowCounts {
    fn default() -> Self {
        RowCounts { default: 50, overrides: Vec::new() }
    }
}

impl RowCounts {
    /// A flat count for every table.
    pub fn flat(default: usize) -> RowCounts {
        RowCounts { default, overrides: Vec::new() }
    }

    /// Parse the repeatable `--rows` argument, which is either a bare number
    /// or `pattern=number`.
    ///
    /// Errors rather than ignoring what it cannot read. A mistyped override
    /// that is silently dropped produces a run that looks like it worked and
    /// did something else, which is the failure this whole tool is against.
    pub fn parse(arguments: &[String]) -> Result<RowCounts, String> {
        let mut out = RowCounts::default();
        let mut saw_default = false;
        for argument in arguments {
            match argument.split_once('=') {
                Some((pattern, count)) => {
                    let count: usize = count.trim().parse().map_err(|_| {
                        format!("--rows {argument}: \"{count}\" is not a number")
                    })?;
                    if pattern.trim().is_empty() {
                        return Err(format!("--rows {argument}: no table named"));
                    }
                    out.overrides.push((pattern.trim().to_string(), count));
                }
                None => {
                    out.default = argument.trim().parse().map_err(|_| {
                        format!("--rows {argument}: expected a number or table=number")
                    })?;
                    saw_default = true;
                }
            }
        }
        let _ = saw_default;
        Ok(out)
    }

    /// The count for one table. The **last** matching override wins, so a
    /// broad pattern can be written first and then narrowed — the order they
    /// were typed in is the order they are read.
    pub fn for_table(&self, id: &TableId) -> usize {
        let qualified = format!("{}.{}", id.schema, id.name);
        self.overrides
            .iter()
            .rev()
            .find(|(pattern, _)| matches(pattern, &id.name) || matches(pattern, &qualified))
            .map(|(_, count)| *count)
            .unwrap_or(self.default)
    }

    /// The largest count anything could be given, for sizing decisions that
    /// have to be made before the per-table figure is known.
    pub fn largest(&self) -> usize {
        self.overrides
            .iter()
            .map(|(_, n)| *n)
            .chain(std::iter::once(self.default))
            .max()
            .unwrap_or(0)
    }

    /// Overrides that matched no table at all.
    ///
    /// Reported rather than ignored: `--rows order=100` against a schema whose
    /// table is `orders` is a typo, and the run that quietly used 50 instead
    /// is the one nobody notices until the results are wrong.
    pub fn unmatched(&self, tables: &[TableId]) -> Vec<String> {
        self.overrides
            .iter()
            .filter(|(pattern, _)| {
                !tables.iter().any(|id| {
                    matches(pattern, &id.name)
                        || matches(pattern, &format!("{}.{}", id.schema, id.name))
                })
            })
            .map(|(pattern, count)| format!("{pattern}={count}"))
            .collect()
    }
}

/// Tables that already hold rows, which is the thing `--allow-nonempty` is
/// about: seeding on top of real data is nearly always an accident.
pub fn already_populated(
    client: &mut postgres::Client,
    tables: &[TableId],
) -> Result<BTreeMap<TableId, i64>, postgres::Error> {
    let mut out = BTreeMap::new();
    for id in tables {
        // LIMIT 1 inside, so this asks "is there anything here" rather than
        // counting every row of a table that might hold millions.
        let sql = format!("SELECT count(*) FROM (SELECT 1 FROM {} LIMIT 1) t", id.quoted());
        if let Ok(row) = client.query_one(&sql, &[]) {
            let n: i64 = row.get(0);
            if n > 0 {
                out.insert(id.clone(), n);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> TableId {
        TableId::new("public", name)
    }

    #[test]
    fn a_star_matches_any_run_including_none() {
        assert!(matches("*", "orders"));
        assert!(matches("order*", "orders"));
        assert!(matches("order*", "order"));
        assert!(matches("*s", "orders"));
        assert!(matches("*der*", "orders"));
        assert!(!matches("order", "orders"));
        assert!(!matches("*x", "orders"));
    }

    #[test]
    fn a_question_mark_matches_exactly_one() {
        assert!(matches("order?", "orders"));
        assert!(!matches("order?", "order"));
        assert!(!matches("order?", "orderss"));
    }

    #[test]
    fn a_pathological_pattern_still_returns() {
        // The case a naive recursive matcher goes exponential on. It has to
        // come back, and it has to come back with the right answer.
        assert!(!matches("*a*a*a*a*a*a*b", &"a".repeat(40)));
        assert!(matches("*a*a*a*b", "aaaaaaaaaaaaaaaab"));
    }

    #[test]
    fn nothing_stated_means_everything() {
        assert!(Selection::default().allows(&id("orders")));
    }

    #[test]
    fn exclude_beats_include_when_both_name_a_table() {
        // The reading that touches fewer tables is the one to take.
        let s = Selection {
            include: vec!["order*".into()],
            exclude: vec!["order_items".into()],
        };
        assert!(s.allows(&id("orders")));
        assert!(!s.allows(&id("order_items")));
        assert!(!s.allows(&id("users")), "not included, so not touched");
    }

    #[test]
    fn a_pattern_matches_the_qualified_name_too() {
        let s = Selection { include: vec!["billing.*".into()], exclude: vec![] };
        assert!(s.allows(&TableId::new("billing", "invoices")));
        assert!(!s.allows(&TableId::new("public", "invoices")));
    }

    #[test]
    fn a_bare_number_sets_the_default_and_a_pattern_overrides_it() {
        let r = RowCounts::parse(&["10".into(), "order*=200".into()]).unwrap();
        assert_eq!(r.default, 10);
        assert_eq!(r.for_table(&id("users")), 10);
        assert_eq!(r.for_table(&id("orders")), 200);
        assert_eq!(r.largest(), 200);
    }

    #[test]
    fn the_last_matching_override_wins_so_broad_can_be_narrowed() {
        let r = RowCounts::parse(&["*=5".into(), "orders=99".into()]).unwrap();
        assert_eq!(r.for_table(&id("users")), 5);
        assert_eq!(r.for_table(&id("orders")), 99);
    }

    #[test]
    fn a_row_count_that_cannot_be_read_is_an_error_not_a_shrug() {
        assert!(RowCounts::parse(&["orders=many".into()]).is_err());
        assert!(RowCounts::parse(&["lots".into()]).is_err());
        assert!(RowCounts::parse(&["=5".into()]).is_err());
    }

    #[test]
    fn an_override_that_matched_nothing_is_reported() {
        let r = RowCounts::parse(&["order=100".into()]).unwrap();
        assert_eq!(r.unmatched(&[id("orders")]), vec!["order=100".to_string()]);
        let r = RowCounts::parse(&["order*=100".into()]).unwrap();
        assert!(r.unmatched(&[id("orders")]).is_empty());
    }
}
