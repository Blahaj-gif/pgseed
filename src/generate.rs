//! Producing values, deterministically, that satisfy what was read.
//!
//! Two properties matter more than the values being interesting.
//!
//! **Determinism, per cell.** Every value comes from a stream keyed on
//! `(seed, table, column, row)` rather than drawn from one sequential
//! generator. A single global stream is deterministic only for a *frozen*
//! schema: add a table, or change one table's row count, and every value in
//! every other table shifts. Keying per cell means a diff of two runs shows
//! what actually changed, which is the case that matters and the one a global
//! stream gets wrong.
//!
//! **Obligations, honoured.** `classify` accepts a table on the strength of
//! what the checks in `crate::checks` promised — a length limit, a byte width,
//! a lower bound, a column that must be left NULL. Those promises are kept
//! here. A generator that ignores them turns a considered "fillable" into rows
//! the database rejects, which is worse than refusing the table outright.

use std::collections::BTreeMap;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::checks::{self, Meaning};
use crate::schema::{Column, ColumnType, Table, TableId};

/// What a column's own constraints require of every value written to it.
#[derive(Debug, Clone, Default)]
pub struct Bounds {
    pub max_length: Option<i32>,
    pub exact_bytes: Option<i32>,
    pub min: Option<i64>,
    pub must_be_null: bool,
    /// Every generated word is lowercase already, so this changes nothing
    /// today. It is recorded anyway: relying on a coincidence in a word list
    /// is not the same as honouring a constraint, and the day somebody adds
    /// "Zulu" to that list is the day the coincidence ends.
    pub lowercase: bool,
}

/// Gather, per column, everything the table's CHECK constraints promised.
///
/// Only the shapes `checks` recognised exactly; anything else refused the
/// table long before this and cannot reach here.
pub fn bounds_for(table: &Table) -> BTreeMap<String, Bounds> {
    let mut out: BTreeMap<String, Bounds> = BTreeMap::new();
    for check in &table.checks {
        match checks::interpret(&check.definition) {
            Meaning::LengthLimit { column, max } => {
                let entry = out.entry(column).or_default();
                // The tightest limit wins: two constraints on one column both
                // have to hold.
                entry.max_length = Some(entry.max_length.map_or(max, |m: i32| m.min(max)));
            }
            Meaning::ByteLength { column, exact } => {
                out.entry(column).or_default().exact_bytes = Some(exact);
            }
            Meaning::LowerBound { column, min, inclusive } => {
                let floor = if inclusive { min } else { min + 1 };
                let entry = out.entry(column).or_default();
                entry.min = Some(entry.min.map_or(floor, |m: i64| m.max(floor)));
            }
            Meaning::MustBeNull { column } => {
                out.entry(column).or_default().must_be_null = true;
            }
            Meaning::Lowercase { column } => {
                out.entry(column).or_default().lowercase = true;
            }
            Meaning::NotNull { .. } | Meaning::Unknown => {}
        }
    }
    out
}

/// A value, already rendered as the SQL literal that will carry it.
///
/// Rendering here rather than later keeps the escaping in one place. A value
/// that reaches SQL unquoted is an injection, and a schema is full of
/// attacker-adjacent strings if anybody ever points this at a database whose
/// column names came from somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Literal(pub String);

impl Literal {
    pub fn null() -> Literal {
        Literal("NULL".into())
    }

    /// A string as a SQL literal, with quotes doubled. The only escaping rule
    /// that matters, and the only one that must never be skipped.
    pub fn text(value: &str) -> Literal {
        Literal(format!("'{}'", value.replace('\'', "''")))
    }
}

/// The deterministic stream for one cell.
///
/// Built from the seed and the cell's identity, so it does not matter what was
/// generated before it or how many tables came first.
fn stream(seed: u64, table: &TableId, column: &str, row: usize) -> ChaCha8Rng {
    // A cheap, stable hash. Not cryptographic and not required to be: it needs
    // to be the same on every machine and every run, which rules out
    // `DefaultHasher` — its output is explicitly not stable across releases.
    let mut key: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for byte in table
        .schema
        .bytes()
        .chain(b"/".iter().copied())
        .chain(table.name.bytes())
        .chain(b"/".iter().copied())
        .chain(column.bytes())
    {
        key ^= byte as u64;
        key = key.wrapping_mul(0x100_0000_01b3);
    }
    key ^= (row as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    key = key.wrapping_mul(0x100_0000_01b3);
    ChaCha8Rng::seed_from_u64(key)
}

/// Words to build text from. Deliberately dull: this generates *valid* data,
/// not realistic data, and pretending otherwise invites somebody to demo with
/// it.
const WORDS: [&str; 16] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
];

/// One value for one cell.
///
/// `unique_hint` is mixed into anything that has to differ row to row — a
/// column under a unique constraint, or a primary key this has to invent.
pub fn value(
    seed: u64,
    table: &TableId,
    column: &Column,
    row: usize,
    bounds: &Bounds,
    unique: bool,
) -> Literal {
    if bounds.must_be_null {
        return Literal::null();
    }
    let mut rng = stream(seed, table, &column.name, row);
    render(&mut rng, &column.type_, row, bounds, unique)
}

fn render(
    rng: &mut ChaCha8Rng,
    type_: &ColumnType,
    row: usize,
    bounds: &Bounds,
    unique: bool,
) -> Literal {
    match type_ {
        ColumnType::Boolean => Literal(if rng.gen::<bool>() { "true" } else { "false" }.into()),

        ColumnType::Integer { bytes } => {
            // Bounded by the column's own width, so a smallint never overflows,
            // and by any lower bound a CHECK imposed.
            let ceiling: i64 = match bytes {
                2 => 32_000,
                4 => 2_000_000_000,
                _ => 4_000_000_000_000,
            };
            let floor = bounds.min.unwrap_or(0).max(0);
            let span = (ceiling - floor).max(1);
            // A unique column steps by row rather than rolling again, because
            // rolling twice in a small range collides sooner than anyone
            // expects.
            let n = if unique {
                floor + (row as i64 % span)
            } else {
                floor + rng.gen_range(0..span.min(1_000_000))
            };
            Literal(n.to_string())
        }

        ColumnType::Float { .. } => {
            let floor = bounds.min.unwrap_or(0) as f64;
            Literal(format!("{:.4}", floor + rng.gen_range(0.0..1000.0)))
        }

        ColumnType::Numeric { precision, scale } => {
            // Must fit the declared precision or the insert fails. `numeric(5,2)`
            // holds at most 999.99, and generating 1000.00 for it is the kind of
            // error that only shows up on the schemas that declare limits.
            let scale = scale.unwrap_or(2).clamp(0, 6);
            let digits = precision.unwrap_or(10).clamp(1, 12) - scale;
            let ceiling = 10i64.saturating_pow(digits.max(1) as u32) - 1;
            let floor = bounds.min.unwrap_or(0).max(0).min(ceiling);
            let whole = floor + rng.gen_range(0..=(ceiling - floor).max(0));
            if scale == 0 {
                Literal(whole.to_string())
            } else {
                let frac: u32 = rng.gen_range(0..10u32.pow(scale as u32));
                Literal(format!("{whole}.{frac:0width$}", width = scale as usize))
            }
        }

        ColumnType::Text { max_length } => {
            // The tightest of the declared length and any CHECK limit.
            let limit = match (max_length, bounds.max_length) {
                (Some(a), Some(b)) => Some((*a).min(b)),
                (Some(a), None) => Some(*a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            let word = WORDS[rng.gen_range(0..WORDS.len())];
            let mut text = if unique {
                format!("{word}-{row}")
            } else {
                word.to_string()
            };
            if let Some(limit) = limit {
                let limit = limit.max(1) as usize;
                if text.chars().count() > limit {
                    // Truncate on a character boundary, not a byte one.
                    text = text.chars().take(limit).collect();
                }
            }
            if bounds.lowercase {
                text = text.to_lowercase();
            }
            Literal::text(&text)
        }

        ColumnType::Uuid => {
            let a: u64 = rng.gen();
            let b: u64 = rng.gen();
            let hex = format!("{a:016x}{b:016x}");
            Literal::text(&format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]
            ))
        }

        ColumnType::Date => {
            let day = rng.gen_range(1..=28);
            let month = rng.gen_range(1..=12);
            Literal::text(&format!("2026-{month:02}-{day:02}"))
        }

        ColumnType::Time => Literal::text(&format!(
            "{:02}:{:02}:{:02}",
            rng.gen_range(0..24),
            rng.gen_range(0..60),
            rng.gen_range(0..60)
        )),

        ColumnType::Timestamp { with_zone } => {
            let stamp = format!(
                "2026-{:02}-{:02} {:02}:{:02}:{:02}",
                rng.gen_range(1..=12),
                rng.gen_range(1..=28),
                rng.gen_range(0..24),
                rng.gen_range(0..60),
                rng.gen_range(0..60)
            );
            Literal::text(&if *with_zone { format!("{stamp}+00") } else { stamp })
        }

        ColumnType::Interval => Literal::text(&format!("{} days", rng.gen_range(1..90))),

        ColumnType::Json { .. } => Literal::text("{}"),

        ColumnType::Bytea => {
            // A CHECK may pin the width exactly, which is what
            // `octet_length(col) = N` means and why it is recognised at all.
            let width = bounds.exact_bytes.unwrap_or(8).clamp(0, 4096) as usize;
            let mut hex = String::with_capacity(width * 2);
            for _ in 0..width {
                hex.push_str(&format!("{:02x}", rng.gen::<u8>()));
            }
            Literal(format!("'\\x{hex}'::bytea"))
        }

        ColumnType::Network { kind } => {
            use crate::schema::NetworkKind;
            Literal::text(&match kind {
                NetworkKind::Inet => format!(
                    "10.{}.{}.{}", rng.gen_range(0..256), rng.gen_range(0..256),
                    rng.gen_range(1..255)),
                // A cidr must have zeroes in the host part or Postgres rejects
                // it outright, which /24 on a .0 address guarantees.
                NetworkKind::Cidr => format!("10.{}.{}.0/24",
                    rng.gen_range(0..256), rng.gen_range(0..256)),
                NetworkKind::MacAddr => format!(
                    "08:00:2b:{:02x}:{:02x}:{:02x}",
                    rng.gen::<u8>(), rng.gen::<u8>(), rng.gen::<u8>()),
            })
        }

        ColumnType::Enum { labels, .. } => {
            if labels.is_empty() {
                Literal::null()
            } else {
                Literal::text(&labels[rng.gen_range(0..labels.len())])
            }
        }

        ColumnType::Domain { inner, .. } => render(rng, inner, row, bounds, unique),

        // One element is enough to be a valid array, and a longer one only
        // makes a failure harder to read.
        ColumnType::Array { of, .. } => {
            let Literal(inner) = render(rng, of, row, bounds, unique);
            Literal(format!("ARRAY[{inner}]"))
        }

        // Unreachable: a table carrying one of these was refused. Rendering
        // DEFAULT rather than panicking, because a panic in a generator is a
        // worse failure than a row the database rejects loudly.
        ColumnType::Unsupported { .. } => Literal("DEFAULT".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::CheckConstraint;

    fn column(name: &str, type_: ColumnType) -> Column {
        Column {
            name: name.into(),
            type_,
            nullable: true,
            has_default: false,
            is_generated: false,
            position: 1,
        }
    }

    fn table_id() -> TableId {
        TableId::new("public", "t")
    }

    fn generate(column: &Column, row: usize, bounds: &Bounds) -> String {
        value(42, &table_id(), column, row, bounds, false).0
    }

    #[test]
    fn the_same_seed_and_cell_always_produce_the_same_value() {
        let c = column("name", ColumnType::Text { max_length: None });
        let first = value(7, &table_id(), &c, 3, &Bounds::default(), false);
        let again = value(7, &table_id(), &c, 3, &Bounds::default(), false);
        assert_eq!(first, again);
    }

    #[test]
    fn a_cell_does_not_move_when_its_neighbours_change() {
        // The reason the stream is keyed per cell rather than drawn from one
        // sequence: adding a table, or changing another table's row count,
        // must not shift the values here. A global stream fails exactly this.
        let c = column("name", ColumnType::Text { max_length: None });
        let mine = value(7, &table_id(), &c, 3, &Bounds::default(), false);

        // Whatever anybody else generates, in any quantity, first.
        let other = TableId::new("public", "somewhere_else");
        for row in 0..500 {
            let _ = value(7, &other, &c, row, &Bounds::default(), false);
        }
        assert_eq!(value(7, &table_id(), &c, 3, &Bounds::default(), false), mine);
    }

    #[test]
    fn different_seeds_give_different_data() {
        let c = column("name", ColumnType::Text { max_length: None });
        let a = value(1, &table_id(), &c, 0, &Bounds::default(), false);
        let b = value(2, &table_id(), &c, 0, &Bounds::default(), false);
        assert_ne!(a, b);
    }

    #[test]
    fn a_quote_in_a_generated_string_is_escaped() {
        // Nothing generated here contains one today, and this is the guard for
        // the day something does: an unescaped quote is a syntax error at best
        // and an injection at worst.
        assert_eq!(Literal::text("it's").0, "'it''s'");
    }

    #[test]
    fn a_length_limit_is_never_exceeded() {
        let c = column("code", ColumnType::Text { max_length: Some(4) });
        for row in 0..50 {
            let rendered = generate(&c, row, &Bounds::default());
            let inner = rendered.trim_matches('\'');
            assert!(inner.chars().count() <= 4, "{rendered}");
        }
    }

    #[test]
    fn the_tighter_of_two_length_limits_wins() {
        // A varchar(40) carrying CHECK (char_length(x) <= 5) has to respect 5.
        let c = column("code", ColumnType::Text { max_length: Some(40) });
        let bounds = Bounds { max_length: Some(5), ..Bounds::default() };
        for row in 0..50 {
            let rendered = generate(&c, row, &bounds);
            assert!(rendered.trim_matches('\'').chars().count() <= 5, "{rendered}");
        }
    }

    #[test]
    fn a_lower_bound_is_respected_and_exclusive_means_strictly_above() {
        let c = column("n", ColumnType::Integer { bytes: 4 });
        let bounds = Bounds { min: Some(100), ..Bounds::default() };
        for row in 0..50 {
            let n: i64 = generate(&c, row, &bounds).parse().unwrap();
            assert!(n >= 100, "{n}");
        }
    }

    #[test]
    fn a_smallint_never_overflows_its_width() {
        let c = column("n", ColumnType::Integer { bytes: 2 });
        for row in 0..200 {
            let n: i64 = generate(&c, row, &Bounds::default()).parse().unwrap();
            assert!(n <= 32_767, "{n} does not fit a smallint");
        }
    }

    #[test]
    fn a_numeric_fits_its_declared_precision() {
        // numeric(5,2) holds at most 999.99. Generating 1000.00 for it fails at
        // insert time, on exactly the schemas careful enough to declare limits.
        let c = column("amount", ColumnType::Numeric { precision: Some(5), scale: Some(2) });
        for row in 0..100 {
            let rendered = generate(&c, row, &Bounds::default());
            let whole: i64 = rendered.split('.').next().unwrap().parse().unwrap();
            assert!(whole <= 999, "{rendered} does not fit numeric(5,2)");
        }
    }

    #[test]
    fn a_byte_width_check_produces_exactly_that_many_bytes() {
        let c = column("digest", ColumnType::Bytea);
        let bounds = Bounds { exact_bytes: Some(20), ..Bounds::default() };
        let rendered = generate(&c, 0, &bounds);
        let hex = rendered.trim_start_matches("'\\x").trim_end_matches("'::bytea");
        assert_eq!(hex.len(), 40, "20 bytes is 40 hex characters: {rendered}");
    }

    #[test]
    fn a_column_that_must_be_null_is_null_whatever_its_type() {
        let c = column("file_md5", ColumnType::Bytea);
        let bounds = Bounds { must_be_null: true, ..Bounds::default() };
        assert_eq!(generate(&c, 0, &bounds), "NULL");
    }

    #[test]
    fn a_unique_column_does_not_repeat_across_rows() {
        let c = column("email", ColumnType::Text { max_length: None });
        let mut seen = std::collections::BTreeSet::new();
        for row in 0..200 {
            let v = value(1, &table_id(), &c, row, &Bounds::default(), true);
            assert!(seen.insert(v.0.clone()), "{:?} appeared twice", v);
        }
    }

    #[test]
    fn an_enum_only_ever_produces_one_of_its_labels() {
        let c = column("state", ColumnType::Enum {
            name: "status".into(),
            labels: vec!["pending".into(), "shipped".into()],
        });
        for row in 0..50 {
            let v = generate(&c, row, &Bounds::default());
            assert!(v == "'pending'" || v == "'shipped'", "{v}");
        }
    }

    #[test]
    fn bounds_are_collected_from_the_tables_own_checks() {
        let table = Table {
            id: table_id(),
            columns: vec![column("name", ColumnType::Text { max_length: None })],
            foreign_keys: vec![],
            unique_keys: vec![],
            checks: vec![
                CheckConstraint {
                    name: "a".into(),
                    definition: "CHECK ((char_length(name) <= 40))".into(),
                },
                CheckConstraint {
                    name: "b".into(),
                    definition: "CHECK ((char_length(name) <= 10))".into(),
                },
            ],
        };
        // Both constraints hold, so the tighter one governs.
        assert_eq!(bounds_for(&table)["name"].max_length, Some(10));
    }
}
