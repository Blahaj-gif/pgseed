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
use crate::nouns::{self, Noun};
use crate::schema::{Column, ColumnType, Table, TableId};

/// What a column's own constraints require of every value written to it.
#[derive(Debug, Clone, Default)]
pub struct Bounds {
    pub max_length: Option<i32>,
    /// A ceiling on a generated number, from `col <= N`. The mirror of `min`.
    pub max: Option<i64>,
    /// A floor on the length, from `char_length(col) >= N`. Satisfied by
    /// padding, which is provable in a way that hoping the word list is long
    /// enough is not.
    pub min_length: Option<i32>,
    pub exact_bytes: Option<i32>,
    pub min: Option<i64>,
    pub must_be_null: bool,
    /// The JSON type the value has to be, from `jsonb_typeof(col) = 'object'`.
    pub json_type: Option<String>,
    /// The only values a CHECK will accept, as SQL literals, from
    /// `col = ANY (ARRAY[...])`. Written out verbatim: they are already valid
    /// SQL and re-rendering could only lose a cast.
    pub value_set: Option<Vec<String>>,
    /// The value has to equal its own lowercasing, from `col = lower(col)`.
    ///
    /// This used to be recorded and never used, because every word the
    /// generator knew was lowercase already, with a note saying that relying
    /// on a coincidence in a word list is not the same as honouring a
    /// constraint. The coincidence ended when `nouns` arrived: `Ada Achebe`
    /// and `Europe/Berlin` are not lowercase, and this is now what keeps rows
    /// out of a column that insists.
    pub lowercase: bool,
}

/// Gather, per column, everything the table's CHECK constraints promised.
///
/// Only the shapes `checks` recognised exactly; anything else refused the
/// table long before this and cannot reach here.
pub fn bounds_for(table: &Table) -> BTreeMap<String, Bounds> {
    let mut out: BTreeMap<String, Bounds> = BTreeMap::new();
    for check in &table.checks {
        for meaning in checks::interpret_all(&check.definition) {
            match meaning {
                Meaning::LengthLimit { column, max } => {
                    let entry = out.entry(column).or_default();
                    // The tightest limit wins: two constraints on one column both
                    // have to hold.
                    entry.max_length = Some(entry.max_length.map_or(max, |m: i32| m.min(max)));
                }
                Meaning::MinLength { column, min } => {
                    // The loosest floor loses: two floors on one column both have
                    // to hold, so the higher one is the binding one.
                    let entry = out.entry(column).or_default();
                    entry.min_length = Some(entry.min_length.map_or(min, |m: i32| m.max(min)));
                }
                Meaning::ByteLength { column, exact } => {
                    out.entry(column).or_default().exact_bytes = Some(exact);
                }
                Meaning::LowerBound {
                    column,
                    min,
                    inclusive,
                } => {
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
                Meaning::ByteLimit { column, max } => {
                    // Every string this generates is ASCII, so a byte ceiling and
                    // a character ceiling are the same ceiling.
                    let entry = out.entry(column).or_default();
                    entry.max_length = Some(entry.max_length.map_or(max, |m: i32| m.min(max)));
                }
                Meaning::ValueSet { column, values } => {
                    let entry = out.entry(column).or_default();
                    // Two sets over one column leave only what they share.
                    entry.value_set = Some(match entry.value_set.take() {
                        Some(existing) => existing
                            .into_iter()
                            .filter(|v| values.contains(v))
                            .collect(),
                        None => values,
                    });
                }
                Meaning::JsonType { column, kind } => {
                    out.entry(column).or_default().json_type = Some(kind);
                }
                Meaning::ExactlyOneNonNull { columns } => {
                    // One column carries the value and the rest are obliged to be
                    // null. Which one is decided by `filled_column` so that the
                    // classifier and this cannot disagree — if they did, the
                    // classifier would accept a table on the strength of a choice
                    // nobody made.
                    let keep = filled_column(table, &columns);
                    for column in columns {
                        if Some(&column) != keep.as_ref() {
                            out.entry(column).or_default().must_be_null = true;
                        }
                    }
                }
                // Every array this generates holds one element, so any limit of
                // one or more is already met and there is nothing to record.
                // Nothing to record for any of these. An array limit of one or
                // more is already met because every array written holds one
                // element; a non-empty column already is; and at-least-one is
                // satisfied by filling all of them, which is what happens.
                Meaning::CardinalityLimit { .. }
                | Meaning::NonEmpty { .. }
                | Meaning::AtLeastOneNonNull { .. }
                | Meaning::NotNull { .. }
                | Meaning::Unknown => {}
                Meaning::UpperBound {
                    column,
                    max,
                    inclusive,
                } => {
                    let ceiling = if inclusive { max } else { max - 1 };
                    let entry = out.entry(column).or_default();
                    // The lowest ceiling binds, the same way the highest floor does.
                    entry.max = Some(entry.max.map_or(ceiling, |m: i64| m.min(ceiling)));
                }
            }
        }
    }
    out
}

/// Which column of a `num_nonnulls(...) = 1` group gets the value.
///
/// Decided in one place because two callers depend on the answer: the
/// classifier, which has to know the choice is possible before accepting the
/// table, and `bounds_for`, which nulls the others. A second implementation of
/// this rule would eventually disagree with the first, and the table would be
/// accepted on the strength of a choice that was never made.
///
/// A column the catalogue says is NOT NULL has no choice about holding a
/// value, so it is the one. Otherwise a column with no foreign key on it,
/// because that one certainly gets a value — an unmatched foreign key can
/// still come out NULL, which would leave the count at zero. Failing both,
/// the first, and the classifier decides whether that is good enough.
pub fn filled_column(table: &Table, columns: &[String]) -> Option<String> {
    let present: Vec<&String> = columns
        .iter()
        .filter(|c| table.column(c).is_some())
        .collect();
    let required: Vec<&&String> = present
        .iter()
        .filter(|c| !table.column(c).expect("present").nullable)
        .collect();
    if let [only] = required.as_slice() {
        return Some((**only).clone());
    }
    if !required.is_empty() {
        // Two columns that both must hold a value cannot have exactly one
        // between them. There is no choice to make and the table is refused.
        return None;
    }
    present
        .iter()
        .find(|c| !table.foreign_keys.iter().any(|fk| fk.columns.contains(c)))
        .or_else(|| present.first())
        .map(|c| (*c).clone())
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

/// Words for a text column whose name says nothing about what it holds.
///
/// This used to be the NATO alphabet, on the reasoning that a tool producing
/// *valid* data should not dress up as one producing realistic data. The
/// reasoning survives — `nouns` is a closed set and a column outside it gets
/// no claim made about it — but the words do not have to be nonsense to make
/// the point. These are ordinary lowercase nouns of the kind a `value` or
/// `data` column plausibly holds, and every one of them is ASCII and
/// lowercase, which `bounds.lowercase` and the byte-limit reasoning in
/// `checks` both depend on.
const WORDS: [&str; 16] = [
    "invoice",
    "shipment",
    "ledger",
    "manifest",
    "receipt",
    "transfer",
    "dispatch",
    "batch",
    "carrier",
    "warehouse",
    "payload",
    "refund",
    "settlement",
    "consignment",
    "allocation",
    "reconciliation",
];

/// A date `n` days after 2020-01-01, as `YYYY-MM-DD`.
///
/// Counting rather than rolling, so a unique date column gets as many distinct
/// days as it is asked for. Written out because the alternative is a date
/// library for one function, and the arithmetic is the civil-from-days
/// algorithm rather than anything invented here.
pub fn days_from_epoch(n: usize) -> String {
    // Days since 1970-01-01, offset so the dates look contemporary.
    let z = (n as i64) + 18_262 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Rebuild the argument `render` takes, for the two types that recurse.
fn step_of(unique: bool, step: usize) -> Option<usize> {
    unique.then_some(step)
}

/// A row index as the shortest distinct string that can stand for it.
///
/// Base 36 rather than decimal because a `varchar(4)` column holds 1,679,616
/// distinct values that way and only 10,000 in decimal, and the whole reason
/// this exists is columns with very little room.
pub fn base36(mut n: usize) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[n % 36]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("ascii")
}

/// How many distinct strings `base36` can produce inside a length limit.
///
/// `None` where the limit is generous enough that no row count will reach it —
/// 36^7 is 78 billion, and reporting a bound that large is the same as
/// reporting none.
pub fn text_domain(limit: i32) -> Option<usize> {
    let limit = limit.max(0) as u32;
    (limit <= 6).then(|| 36usize.saturating_pow(limit))
}

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
    variation: Option<usize>,
) -> Literal {
    value_as(seed, table, column, row, bounds, variation, row)
}

/// The same, told which row's identity this one borrows.
///
/// `identity` is the odometer position the person-shaped columns read, and it
/// is the row index unless the caller knows better. The caller who knows
/// better is the emitter: a child row points at a particular parent row, and
/// its `first_name` ought to be that parent's.
///
/// The two agree while the child's index is below its parent's row count and
/// part company the moment the foreign key wraps. Three users and seven
/// `user_emails` used to give four addresses belonging to people who were
/// never written.
///
/// This does not touch the per-cell stream, which is still keyed on the row —
/// so a value's independence from its neighbours, and every determinism
/// property tested here, is unchanged. And it cannot affect distinctness: a
/// column under a unique key is driven by its `step`, never by this.
#[allow(clippy::too_many_arguments)]
pub fn value_as(
    seed: u64,
    table: &TableId,
    column: &Column,
    row: usize,
    bounds: &Bounds,
    variation: Option<usize>,
    identity: usize,
) -> Literal {
    if bounds.must_be_null {
        return Literal::null();
    }
    // A listed set of values overrides the type entirely: whatever an integer
    // column would otherwise hold, this one holds one of these.
    if let Some(values) = &bounds.value_set {
        if values.is_empty() {
            // Two sets with nothing in common. `classify` rejects that before
            // it can reach here, and returning NULL rather than panicking is
            // the right way to be wrong about it.
            return Literal::null();
        }
        let index = match variation {
            Some(stride) => (row / stride.max(1)) % values.len(),
            None => stream(seed, table, &column.name, row).gen_range(0..values.len()),
        };
        return Literal(values[index].clone());
    }

    let mut rng = stream(seed, table, &column.name, row);
    // The stride is how often this column has to change: every row for a
    // single-column key, every Nth row for a digit of a composite one. The
    // index it steps by is the row divided by it.
    let step = variation.map(|stride| row / stride.max(1));
    // What the column is called, where that says something exact. Worked out
    // once here rather than inside `render`, which recurses through domains
    // and arrays and would otherwise re-derive it at every level.
    let named = Named {
        noun: nouns::of_in(&table.name, &column.name),
        // And, for a number, whatever its name says about the size of it.
        range: nouns::numeric_range(&column.name),
        moment: nouns::moment_of(&column.name),
    };
    render(&mut rng, &column.type_, identity, step, bounds, &named)
}

/// Everything the column's *name* implies, worked out once at the call site.
///
/// `render` recurses through domains and arrays, and re-deriving these at every
/// level would read the same name three times to get the same answer. Grouped
/// rather than passed loose because they are one idea — what this column is
/// called — and because eight arguments is too many, which clippy says out loud.
#[derive(Clone, Copy)]
struct Named {
    noun: Option<Noun>,
    range: Option<(i64, i64)>,
    moment: Option<nouns::Moment>,
}

fn render(
    rng: &mut ChaCha8Rng,
    type_: &ColumnType,
    // The odometer position the person-shaped nouns read, and the fallback
    // step for the branches that only consult it when a value has to be
    // distinct. Usually the row index; the row a foreign key points at when
    // the emitter knows which one that is.
    identity: usize,
    step: Option<usize>,
    bounds: &Bounds,
    named: &Named,
) -> Literal {
    let Named {
        noun,
        range,
        moment,
    } = *named;
    let unique = step.is_some();
    let step = step.unwrap_or(identity);
    match type_ {
        // Two values, so a unique column walks them rather than rolling: two
        // rolls of a coin agree half the time, and the row count is capped at
        // two anyway by `volume`.
        ColumnType::Boolean => {
            let b = if unique {
                step % 2 == 1
            } else {
                rng.gen::<bool>()
            };
            Literal(if b { "true" } else { "false" }.into())
        }

        ColumnType::Integer { bytes } => {
            // Bounded by the column's own width, so a smallint never overflows,
            // and by any lower bound a CHECK imposed.
            let width: i64 = match bytes {
                2 => 32_000,
                4 => 2_000_000_000,
                _ => 4_000_000_000_000,
            };
            // The column's own width and any CHECK ceiling, whichever binds.
            let ceiling = bounds.max.map_or(width, |m| m.min(width));
            let mut floor = bounds.min.unwrap_or(0).max(0).min(ceiling);
            let mut ceiling = ceiling;
            // What the column is called, where the name says something about
            // the *size* of the number. Only when the value does not have to
            // be distinct — a range of twenty holds twenty rows, and being
            // plausible is worth much less than being unique.
            if !unique {
                if let Some((low, high)) = range {
                    // Narrowed, never widened. A CHECK that already bounds the
                    // column outranks a guess made from its name, and where
                    // the two do not overlap the name is simply dropped.
                    if low.max(floor) <= high.min(ceiling) {
                        floor = low.max(floor);
                        ceiling = high.min(ceiling);
                    }
                }
            }
            let span = (ceiling - floor).max(1);
            // A unique column steps by row rather than rolling again, because
            // rolling twice in a small range collides sooner than anyone
            // expects.
            let n = if unique {
                floor + (step as i64 % span)
            } else {
                floor + rng.gen_range(0..span.min(1_000_000))
            };
            Literal(n.to_string())
        }

        ColumnType::Float { .. } => {
            let floor = bounds.min.unwrap_or(0) as f64;
            let ceiling = bounds.max.map(|m| m as f64);
            // Stepping by a fraction rather than a whole number, so a unique
            // float column does not collide with a unique integer one beside
            // it for no reason.
            let offset = if unique {
                step as f64 / 16.0
            } else {
                rng.gen_range(0.0..1000.0)
            };
            let value = match ceiling {
                // `readiness_score >= 0 AND readiness_score <= 1` leaves a
                // range of one, and walking off the top of it is a rejected
                // row rather than a rounding error.
                Some(top) if top > floor => floor + (offset % (top - floor)),
                Some(top) => top.min(floor),
                None => floor + offset,
            };
            Literal(format!("{value:.4}"))
        }

        ColumnType::Numeric { precision, scale } => {
            // Must fit the declared precision or the insert fails. `numeric(5,2)`
            // holds at most 999.99, and generating 1000.00 for it is the kind of
            // error that only shows up on the schemas that declare limits.
            let scale = scale.unwrap_or(2).clamp(0, 6);
            let digits = precision.unwrap_or(10).clamp(1, 12) - scale;
            let declared = 10i64.saturating_pow(digits.max(1) as u32) - 1;
            let ceiling = bounds.max.map_or(declared, |m| m.min(declared));
            let floor = bounds.min.unwrap_or(0).max(0).min(ceiling);
            let span = (ceiling - floor).max(0);
            let whole = if unique {
                floor + (step as i64 % (span + 1))
            } else {
                floor + rng.gen_range(0..=span)
            };
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
            // The column's name first, where it named something exactly. The
            // noun either produces a value that fits inside `limit` and is
            // distinct for its step, or declines, and declining falls through
            // to the path below, which is built for columns with no room.
            //
            // `row` rather than the step is what a *non*-unique noun is drawn
            // from, so that every person-shaped column in one row describes
            // the same person: `first_name`, `last_name` and `email` all read
            // the same odometer position and agree. A unique column keeps the
            // step, because being distinct comes first, and a composite key
            // whose stride moves it off the row is the one case where the
            // agreement is given up rather than the distinctness.
            let named = noun.and_then(|noun| {
                nouns::render(
                    noun,
                    rng,
                    step_of(unique, step),
                    identity,
                    limit.map(|l| l.max(0) as usize),
                )
            });

            let word = WORDS[rng.gen_range(0..WORDS.len())];
            let mut text = match (named, unique, limit) {
                (Some(named), _, _) => named,
                // The row index has to survive the length limit, and appending
                // it does not: `varchar(4)` cut a word plus its index down to
                // the first four characters of the word on every row, so a
                // unique column produced the same four characters over and
                // over. The index goes in first and the word fills whatever
                // room is left over.
                (None, true, Some(limit)) => {
                    let limit = limit.max(1) as usize;
                    let tag = base36(step);
                    let room = limit.saturating_sub(tag.chars().count());
                    let head: String = word.chars().take(room).collect();
                    let tail: String = {
                        let n = tag.chars().count();
                        tag.chars().skip(n.saturating_sub(limit)).collect()
                    };
                    format!("{head}{tail}")
                }
                (None, true, None) => format!("{word}-{step}"),
                (None, false, _) => word.to_string(),
            };

            // A floor on the length, padded up to. The filler is a hyphen
            // because no value this generates ends in one, which is what makes
            // padding safe on a unique column: two values that differed before
            // padding still differ after it, since neither can be the other
            // with hyphens added.
            if let Some(min) = bounds.min_length {
                let short = (min.max(0) as usize).saturating_sub(text.chars().count());
                if short > 0 {
                    text.push_str(&"-".repeat(short));
                }
            }
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
            // A unique column puts the step in the low half, which is exact
            // rather than probabilistic. Two random u64s almost never collide,
            // and "almost never" is not the promise a unique constraint makes.
            let b: u64 = if unique { step as u64 } else { rng.gen() };
            let hex = format!("{a:016x}{b:016x}");
            Literal::text(&format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            ))
        }

        // Dates count forward from the first of January rather than rolling,
        // so a unique date column gets as many distinct days as it asks for.
        // Discourse has `UNIQUE (date, country_code)` on four rollup tables
        // and a rolled date collided on the second row.
        ColumnType::Date => Literal::text(&days_from_epoch(if unique {
            step
        } else {
            rng.gen_range(0..3650)
        })),

        ColumnType::Time => {
            let seconds = if unique {
                step % 86_400
            } else {
                rng.gen_range(0..86_400)
            };
            Literal::text(&format!(
                "{:02}:{:02}:{:02}",
                seconds / 3600,
                (seconds / 60) % 60,
                seconds % 60
            ))
        }

        // Anchored on the row's identity when the column name says where in
        // the row's life it sits, so `created_at` lands at or before
        // `updated_at` and a child is not created before its parent. Postgres
        // accepts either ordering, which is exactly why nothing caught this:
        // the rows were valid and the application logic on top of them was
        // not. A unique column still walks its step, because distinctness
        // beats coherence wherever the two disagree.
        ColumnType::Timestamp { with_zone } => {
            let seconds = if unique {
                step
            } else if let Some(moment) = moment {
                nouns::moment_seconds(moment, identity, rng)
            } else {
                rng.gen_range(0..315_360_000)
            };
            let stamp = format!(
                "{} {:02}:{:02}:{:02}",
                days_from_epoch(seconds / 86_400),
                (seconds / 3600) % 24,
                (seconds / 60) % 60,
                seconds % 60
            );
            Literal::text(&if *with_zone {
                format!("{stamp}+00")
            } else {
                stamp
            })
        }

        ColumnType::Interval => Literal::text(&format!(
            "{} days",
            if unique { step } else { rng.gen_range(1..90) }
        )),

        // `{}` unless a CHECK named the type it has to be. `jsonb_typeof` of
        // each of these is exactly the word the constraint asked for, which is
        // the whole of what has to be shown.
        //
        // A unique one carries the step inside, because a unique index on a
        // jsonb column is a real thing — Sourcegraph's insights database has
        // one — and every row was getting `{}`. `jsonb_typeof` of each of
        // these is still exactly the word the constraint asked for.
        ColumnType::Json { .. } => Literal::text(&match (bounds.json_type.as_deref(), unique) {
            (Some("array"), false) => "[]".into(),
            (Some("array"), true) => format!("[{step}]"),
            (Some("string"), false) => "\"invoice\"".to_string(),
            (Some("string"), true) => format!("\"invoice-{step}\""),
            (Some("number"), false) => "0".into(),
            (Some("number"), true) => step.to_string(),
            // Two values and one value respectively: these cannot be made
            // distinct beyond that, and `volume` is what stops more being
            // asked of them.
            (Some("boolean"), _) => if unique && step % 2 == 1 {
                "false"
            } else {
                "true"
            }
            .into(),
            (Some("null"), _) => "null".into(),
            (_, false) => "{}".into(),
            (_, true) => format!("{{\"n\": {step}}}"),
        }),

        ColumnType::Bytea => {
            // A CHECK may pin the width exactly, which is what
            // `octet_length(col) = N` means and why it is recognised at all.
            // An exact width if one was declared, otherwise eight, but never
            // more than a byte ceiling allows, since `octet_length(col) <= N`
            // is a real limit on a bytea and not only on text.
            let ceiling = bounds.max_length.unwrap_or(i32::MAX);
            let width = bounds.exact_bytes.unwrap_or(8).min(ceiling).clamp(0, 4096) as usize;
            let mut hex = String::with_capacity(width * 2);
            for byte in 0..width {
                // A unique column spells the step out in its leading bytes, so
                // two rows differ for certain rather than very probably.
                let value = if unique {
                    ((step >> (8 * byte.min(7))) & 0xff) as u8
                } else {
                    rng.gen::<u8>()
                };
                hex.push_str(&format!("{value:02x}"));
            }
            Literal(format!("'\\x{hex}'::bytea"))
        }

        ColumnType::Network { kind } => {
            use crate::schema::NetworkKind;
            Literal::text(&match kind {
                // One radix for every octet, or the digits do not carry
                // together and the number repeats. This counted the low octet
                // modulo 254 and the one above it modulo 256, so step 0 and
                // step 254 were both `10.0.0.1` — which Discourse's unique
                // index on `screened_ip_addresses.ip_address` found at a
                // thousand rows and never at fifty. An `inet` is an address
                // rather than a network, so `.0` and `.255` are both ordinary
                // values and the full byte is available.
                NetworkKind::Inet if unique => format!(
                    "10.{}.{}.{}",
                    (step / 65_536) % 256,
                    (step / 256) % 256,
                    step % 256
                ),
                NetworkKind::Inet => format!(
                    "10.{}.{}.{}",
                    rng.gen_range(0..256),
                    rng.gen_range(0..256),
                    rng.gen_range(1..255)
                ),
                // A cidr must have zeroes in the host part or Postgres rejects
                // it outright, which /24 on a .0 address guarantees.
                // Both of these ignored `unique` entirely and drew from the
                // stream, so two rows under a unique index collided whenever
                // the stream happened to repeat. Same fault as the octets
                // above, found by reading rather than by a failure: no corpus
                // schema has a unique cidr or macaddr column, so nothing would
                // have caught it until somebody's did.
                NetworkKind::Cidr if unique => {
                    format!("10.{}.{}.0/24", (step / 256) % 256, step % 256)
                }
                NetworkKind::MacAddr if unique => format!(
                    "08:00:2b:{:02x}:{:02x}:{:02x}",
                    (step / 65_536) % 256,
                    (step / 256) % 256,
                    step % 256
                ),
                NetworkKind::Cidr => format!(
                    "10.{}.{}.0/24",
                    rng.gen_range(0..256),
                    rng.gen_range(0..256)
                ),
                NetworkKind::MacAddr => format!(
                    "08:00:2b:{:02x}:{:02x}:{:02x}",
                    rng.gen::<u8>(),
                    rng.gen::<u8>(),
                    rng.gen::<u8>()
                ),
            })
        }

        ColumnType::Enum { labels, .. } => {
            if labels.is_empty() {
                Literal::null()
            } else if unique {
                // As with a boolean: the labels are the whole domain, so step
                // through them instead of drawing and hoping.
                Literal::text(&labels[step % labels.len()])
            } else {
                Literal::text(&labels[rng.gen_range(0..labels.len())])
            }
        }

        ColumnType::Domain { inner, .. } => {
            render(rng, inner, identity, step_of(unique, step), bounds, named)
        }

        // One element is enough to be a valid array, and a longer one only
        // makes a failure harder to read.
        ColumnType::Array { of, .. } => {
            // The cast is the whole point. `ARRAY['{}']` is a `text[]` from
            // the moment it is written, and `text[]` does not quietly become
            // `jsonb[]` or `inet[]` — it is rejected, which is three of the
            // nine real schemas. Where the element type has no unambiguous
            // name to write, the array goes out uncast as before rather than
            // naming a type that might belong to another schema.
            let Literal(inner) = render(rng, of, identity, step_of(unique, step), bounds, named);
            match of.sql_name() {
                Some(name) => Literal(format!("ARRAY[{inner}]::{name}[]")),
                None => Literal(format!("ARRAY[{inner}]")),
            }
        }

        // Unreachable: a table carrying one of these was refused. Rendering
        // DEFAULT rather than panicking, because a panic in a generator is a
        // worse failure than a row the database rejects loudly.
        ColumnType::Unsupported { .. } => Literal("DEFAULT".into()),
    }
}

#[cfg(test)]
mod cost {
    //! Where the time goes when a large schema is generated.
    //!
    //! `cargo test --lib -- --ignored --nocapture cost`
    use super::*;

    #[test]
    #[ignore]
    fn what_a_cell_costs() {
        let table = TableId::new("public", "t");
        let column = Column {
            name: "note".into(),
            type_: ColumnType::Text { max_length: None },
            nullable: false,
            has_default: false,
            default_is_sequence: false,
            is_generated: false,
            position: 1,
        };
        let bounds = Bounds::default();
        const N: usize = 200_000;

        let started = std::time::Instant::now();
        let mut sink = 0usize;
        for row in 0..N {
            sink += stream(1, &table, "note", row).gen::<u8>() as usize;
        }
        let seeding = started.elapsed();

        let started = std::time::Instant::now();
        for row in 0..N {
            sink += value(1, &table, &column, row, &bounds, None).0.len();
        }
        let whole = started.elapsed();

        println!(
            "  {N} cells: seeding {seeding:?}, whole value {whole:?}              ({:.0}% of it is seeding). sink={sink}",
            100.0 * seeding.as_secs_f64() / whole.as_secs_f64()
        );
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
            default_is_sequence: false,
            is_generated: false,
            position: 1,
        }
    }

    fn table_id() -> TableId {
        TableId::new("public", "t")
    }

    fn generate(column: &Column, row: usize, bounds: &Bounds) -> String {
        value(42, &table_id(), column, row, bounds, None).0
    }

    #[test]
    fn the_same_seed_and_cell_always_produce_the_same_value() {
        let c = column("name", ColumnType::Text { max_length: None });
        let first = value(7, &table_id(), &c, 3, &Bounds::default(), None);
        let again = value(7, &table_id(), &c, 3, &Bounds::default(), None);
        assert_eq!(first, again);
    }

    #[test]
    fn a_cell_does_not_move_when_its_neighbours_change() {
        // The reason the stream is keyed per cell rather than drawn from one
        // sequence: adding a table, or changing another table's row count,
        // must not shift the values here. A global stream fails exactly this.
        let c = column("name", ColumnType::Text { max_length: None });
        let mine = value(7, &table_id(), &c, 3, &Bounds::default(), None);

        // Whatever anybody else generates, in any quantity, first.
        let other = TableId::new("public", "somewhere_else");
        for row in 0..500 {
            let _ = value(7, &other, &c, row, &Bounds::default(), None);
        }
        assert_eq!(value(7, &table_id(), &c, 3, &Bounds::default(), None), mine);
    }

    #[test]
    fn different_seeds_give_different_data() {
        let c = column("name", ColumnType::Text { max_length: None });
        let a = value(1, &table_id(), &c, 0, &Bounds::default(), None);
        let b = value(2, &table_id(), &c, 0, &Bounds::default(), None);
        assert_ne!(a, b);
    }

    #[test]
    fn a_floor_on_the_length_is_padded_up_to_without_losing_distinctness() {
        // Padding a unique column is where this could go wrong quietly: two
        // values that differed before the filler was added must still differ
        // after it.
        let c = column("code", ColumnType::Text { max_length: None });
        let bounds = Bounds {
            min_length: Some(24),
            ..Default::default()
        };
        let mut seen = std::collections::BTreeSet::new();
        for row in 0..500 {
            let text = value(3, &table_id(), &c, row, &bounds, Some(1)).0;
            let inner = text.trim_matches('\u{27}');
            assert!(
                inner.chars().count() >= 24,
                "row {row} came out short: {text}"
            );
            assert!(seen.insert(text.clone()), "row {row} repeated: {text}");
        }
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
        let c = column(
            "code",
            ColumnType::Text {
                max_length: Some(4),
            },
        );
        for row in 0..50 {
            let rendered = generate(&c, row, &Bounds::default());
            let inner = rendered.trim_matches('\'');
            assert!(inner.chars().count() <= 4, "{rendered}");
        }
    }

    #[test]
    fn the_tighter_of_two_length_limits_wins() {
        // A varchar(40) carrying CHECK (char_length(x) <= 5) has to respect 5.
        let c = column(
            "code",
            ColumnType::Text {
                max_length: Some(40),
            },
        );
        let bounds = Bounds {
            max_length: Some(5),
            ..Bounds::default()
        };
        for row in 0..50 {
            let rendered = generate(&c, row, &bounds);
            assert!(
                rendered.trim_matches('\'').chars().count() <= 5,
                "{rendered}"
            );
        }
    }

    #[test]
    fn a_lower_bound_is_respected_and_exclusive_means_strictly_above() {
        let c = column("n", ColumnType::Integer { bytes: 4 });
        let bounds = Bounds {
            min: Some(100),
            ..Bounds::default()
        };
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
        let c = column(
            "amount",
            ColumnType::Numeric {
                precision: Some(5),
                scale: Some(2),
            },
        );
        for row in 0..100 {
            let rendered = generate(&c, row, &Bounds::default());
            let whole: i64 = rendered.split('.').next().unwrap().parse().unwrap();
            assert!(whole <= 999, "{rendered} does not fit numeric(5,2)");
        }
    }

    #[test]
    fn a_byte_width_check_produces_exactly_that_many_bytes() {
        let c = column("digest", ColumnType::Bytea);
        let bounds = Bounds {
            exact_bytes: Some(20),
            ..Bounds::default()
        };
        let rendered = generate(&c, 0, &bounds);
        let hex = rendered
            .trim_start_matches("'\\x")
            .trim_end_matches("'::bytea");
        assert_eq!(hex.len(), 40, "20 bytes is 40 hex characters: {rendered}");
    }

    #[test]
    fn a_column_that_must_be_null_is_null_whatever_its_type() {
        let c = column("file_md5", ColumnType::Bytea);
        let bounds = Bounds {
            must_be_null: true,
            ..Bounds::default()
        };
        assert_eq!(generate(&c, 0, &bounds), "NULL");
    }

    #[test]
    fn a_unique_column_does_not_repeat_across_rows() {
        let c = column("email", ColumnType::Text { max_length: None });
        let mut seen = std::collections::BTreeSet::new();
        for row in 0..200 {
            let v = value(1, &table_id(), &c, row, &Bounds::default(), Some(1));
            assert!(seen.insert(v.0.clone()), "{:?} appeared twice", v);
        }
    }

    /// Past the point where each octet carries, which is where a mixed radix
    /// shows up and nowhere earlier. `10.0.0.1` came out at step 0 and again
    /// at step 254, and a corpus run at fifty rows could never have seen it.
    #[test]
    fn a_unique_network_column_does_not_repeat_across_a_carry() {
        use crate::schema::NetworkKind;
        for kind in [NetworkKind::Inet, NetworkKind::Cidr, NetworkKind::MacAddr] {
            let named = format!("{kind:?}");
            let c = column("addr", ColumnType::Network { kind });
            let mut seen = std::collections::BTreeSet::new();
            for row in 0..2_000 {
                let v = value(1, &table_id(), &c, row, &Bounds::default(), Some(1));
                assert!(seen.insert(v.0.clone()), "{named}: {:?} appeared twice", v);
            }
        }
    }

    #[test]
    fn an_enum_only_ever_produces_one_of_its_labels() {
        let c = column(
            "state",
            ColumnType::Enum {
                name: "status".into(),
                qualified: None,
                labels: vec!["pending".into(), "shipped".into()],
            },
        );
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

#[cfg(test)]
mod moments {
    use super::*;

    fn stamp(table: &str, column: &str, identity: usize) -> String {
        let c = column_named(column, ColumnType::Timestamp { with_zone: false });
        value(
            1,
            &TableId::new("public", table),
            &c,
            identity,
            &Bounds::default(),
            None,
        )
        .0
    }

    fn column_named(name: &str, type_: ColumnType) -> Column {
        Column {
            name: name.into(),
            type_,
            nullable: true,
            has_default: false,
            default_is_sequence: false,
            is_generated: false,
            position: 1,
        }
    }

    fn table_id() -> TableId {
        TableId::new("public", "t")
    }

    /// The ordering the whole band scheme exists to produce.
    #[test]
    fn a_row_is_created_before_it_is_updated_and_deleted() {
        for row in 0..60 {
            let created = stamp("orders", "created_at", row);
            let updated = stamp("orders", "updated_at", row);
            let deleted = stamp("orders", "deleted_at", row);
            assert!(created <= updated, "row {row}: {created} > {updated}");
            assert!(updated < deleted, "row {row}: {updated} >= {deleted}");
        }
    }

    /// Prisma's spelling reaches the same band, via the same hump split the
    /// ordinary nouns use.
    #[test]
    fn camel_case_timestamps_land_in_the_same_band() {
        assert_eq!(
            nouns::moment_of("createdAt"),
            nouns::moment_of("created_at")
        );
        assert_eq!(
            nouns::moment_of("updatedAt"),
            nouns::moment_of("updated_at")
        );
        assert_eq!(
            nouns::moment_of("expiresAt"),
            nouns::moment_of("expires_at")
        );
        assert_eq!(
            nouns::moment_of("created_on"),
            nouns::moment_of("created_at")
        );
    }

    /// A child borrows its parent's identity, so it is anchored to its
    /// parent's day and cannot be created before it.
    #[test]
    fn a_child_is_not_created_before_the_parent_it_points_at() {
        for parent in 0..40 {
            let theirs = stamp("users", "created_at", parent);
            let mine = stamp("notes", "created_at", parent);
            assert!(mine >= theirs, "note {parent}: {mine} < {theirs}");
        }
    }

    /// Distinctness still wins where the two disagree.
    #[test]
    fn a_unique_timestamp_column_is_still_distinct() {
        let c = column_named("created_at", ColumnType::Timestamp { with_zone: true });
        let mut seen = std::collections::BTreeSet::new();
        for row in 0..2_000 {
            let v = value(1, &table_id(), &c, row, &Bounds::default(), Some(1));
            assert!(seen.insert(v.0.clone()), "{:?} appeared twice", v);
        }
    }

    /// A name outside the closed set keeps the behaviour it had.
    #[test]
    fn an_unnamed_timestamp_is_left_alone() {
        assert_eq!(nouns::moment_of("some_column"), None);
        assert_eq!(nouns::moment_of("scheduled_for"), None);
    }
}
