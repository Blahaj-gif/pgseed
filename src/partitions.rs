//! Whether a row written to a partitioned table lands anywhere.
//!
//! A partitioned table has no storage of its own. Every row must fall inside
//! some partition's bounds, and one that does not is refused —
//! `no partition of relation ... found for row` — however carefully every
//! constraint was satisfied. So a partitioned parent was not read at all, and
//! everything referencing one was refused for pointing at a table nobody had
//! looked at. GitLab has 101 of them.
//!
//! Reading them turns out to be mostly easy, because of how they are
//! partitioned. Counted across the corpus:
//!
//! ```text
//!   339  FOR VALUES WITH (modulus N, remainder N)   hash
//!    33  FOR VALUES IN ('...')                      list
//! ```
//!
//! **Hash partitioning constrains nothing.** If the partitions cover the whole
//! space — every remainder present for one modulus — then any value at all
//! lands in exactly one of them, and the table can be filled like any other.
//! That is nine tenths of the real cases and it needs no generated value to
//! change.
//!
//! **List partitioning constrains one column to a set**, which is the shape
//! `checks` already knows as `col = ANY (ARRAY[...])`.
//!
//! Anything else — range bounds, an incomplete hash, a table with no
//! partitions at all — is refused, because a row that lands nowhere is a row
//! the database throws out.

/// What a partitioned table requires of the rows written to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routing {
    /// Every row lands somewhere, whatever it holds.
    Anything,
    /// The key column must hold one of these, as SQL literals.
    OneOf { column: String, values: Vec<String> },
    /// No row can be shown to land anywhere.
    Unknown,
}

/// Work out the routing from the partition key and the partitions' bounds.
///
/// `key` is what `pg_get_partkeydef` returned — `HASH (id)`, `LIST (kind)` —
/// and `bounds` is `pg_get_expr(relpartbound, oid)` for each partition.
pub fn interpret(key: &str, bounds: &[String]) -> Routing {
    if bounds.is_empty() {
        // A partitioned table with nothing under it takes no rows at all.
        return Routing::Unknown;
    }

    let key = key.trim();
    let Some(column) = key
        .split_once('(')
        .and_then(|(_, rest)| rest.rsplit_once(')'))
        .map(|(inner, _)| inner.trim())
    else {
        return Routing::Unknown;
    };
    // One key column only. A composite partition key is a different question
    // and this does not answer it.
    if column.contains(',') {
        return Routing::Unknown;
    }

    if key.to_uppercase().starts_with("HASH") {
        return if hash_covers_everything(bounds) {
            Routing::Anything
        } else {
            Routing::Unknown
        };
    }

    if key.to_uppercase().starts_with("LIST") {
        let mut values = Vec::new();
        for bound in bounds {
            // `FOR VALUES IN ('a', 'b')`, and `DEFAULT` for the catch-all —
            // which takes anything the others refused, so the table is then
            // unconstrained.
            let upper = bound.trim().to_uppercase();
            if upper == "DEFAULT" {
                return Routing::Anything;
            }
            let Some(list) = bound
                .trim()
                .strip_prefix("FOR VALUES IN (")
                .and_then(|rest| rest.strip_suffix(')'))
            else {
                return Routing::Unknown;
            };
            values.extend(split_values(list));
        }
        if values.is_empty() {
            return Routing::Unknown;
        }
        return Routing::OneOf {
            column: column.trim_matches('"').to_string(),
            values,
        };
    }

    // Range partitioning, or something newer. Not read.
    Routing::Unknown
}

/// Whether a set of hash bounds leaves no value unaccounted for.
///
/// Every partition of a hash-partitioned table declares the same modulus in
/// practice, and the remainders must then be the whole of `0..modulus`. Mixed
/// moduli are legal and rare, and are not read: a gap in the coverage is a row
/// the database refuses, and guessing is the one thing not on offer.
fn hash_covers_everything(bounds: &[String]) -> bool {
    let mut modulus = None;
    let mut remainders = std::collections::BTreeSet::new();

    for bound in bounds {
        let Some((m, r)) = parse_hash(bound) else {
            return false;
        };
        if *modulus.get_or_insert(m) != m {
            return false;
        }
        remainders.insert(r);
    }

    let Some(modulus) = modulus else { return false };
    modulus > 0 && remainders.len() == modulus && remainders.iter().all(|r| *r < modulus)
}

/// `FOR VALUES WITH (modulus 8, remainder 3)` as `(8, 3)`.
fn parse_hash(bound: &str) -> Option<(usize, usize)> {
    let inner = bound
        .trim()
        .strip_prefix("FOR VALUES WITH (")?
        .strip_suffix(')')?;
    let (left, right) = inner.split_once(',')?;
    let modulus = left.trim().strip_prefix("modulus")?.trim().parse().ok()?;
    let remainder = right
        .trim()
        .strip_prefix("remainder")?
        .trim()
        .parse()
        .ok()?;
    Some((modulus, remainder))
}

/// Split a list of SQL literals at the top level, keeping them verbatim.
fn split_values(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0usize, false);
    for (index, ch) in list.char_indices() {
        match ch {
            '\u{27}' => quoted = !quoted,
            ',' if !quoted => {
                out.push(list[start..index].trim().to_string());
                start = index + 1;
            }
            _ => {}
        }
    }
    out.push(list[start..].trim().to_string());
    out.retain(|v| !v.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_complete_hash_constrains_nothing() {
        assert_eq!(
            interpret(
                "HASH (id)",
                &bounds(&[
                    "FOR VALUES WITH (modulus 3, remainder 0)",
                    "FOR VALUES WITH (modulus 3, remainder 1)",
                    "FOR VALUES WITH (modulus 3, remainder 2)",
                ])
            ),
            Routing::Anything
        );
    }

    #[test]
    fn an_incomplete_hash_is_a_row_that_might_land_nowhere() {
        // Two of three remainders: a third of all values fall through.
        assert_eq!(
            interpret(
                "HASH (id)",
                &bounds(&[
                    "FOR VALUES WITH (modulus 3, remainder 0)",
                    "FOR VALUES WITH (modulus 3, remainder 1)",
                ])
            ),
            Routing::Unknown
        );
        // Mixed moduli are legal and are not read.
        assert_eq!(
            interpret(
                "HASH (id)",
                &bounds(&[
                    "FOR VALUES WITH (modulus 2, remainder 0)",
                    "FOR VALUES WITH (modulus 4, remainder 1)",
                ])
            ),
            Routing::Unknown
        );
    }

    #[test]
    fn a_list_gives_the_column_the_values_it_may_hold() {
        assert_eq!(
            interpret(
                "LIST (uploader_type)",
                &bounds(&[
                    "FOR VALUES IN ('AbuseReport')",
                    "FOR VALUES IN ('Achievement', 'Badge')",
                ])
            ),
            Routing::OneOf {
                column: "uploader_type".into(),
                values: vec![
                    "'AbuseReport'".into(),
                    "'Achievement'".into(),
                    "'Badge'".into()
                ],
            }
        );
    }

    #[test]
    fn a_default_partition_takes_whatever_the_others_refused() {
        assert_eq!(
            interpret("LIST (kind)", &bounds(&["FOR VALUES IN ('a')", "DEFAULT"])),
            Routing::Anything
        );
    }

    #[test]
    fn what_is_not_read_is_said_so_rather_than_guessed() {
        // A range, a composite key, and a partitioned table with nothing
        // under it — each is a row that might land nowhere.
        assert_eq!(
            interpret(
                "RANGE (created_at)",
                &bounds(&["FOR VALUES FROM ('2020-01-01') TO ('2021-01-01')"])
            ),
            Routing::Unknown
        );
        assert_eq!(
            interpret("LIST (a, b)", &bounds(&["FOR VALUES IN (('x', 1))"])),
            Routing::Unknown
        );
        assert_eq!(interpret("HASH (id)", &[]), Routing::Unknown);
    }
}
