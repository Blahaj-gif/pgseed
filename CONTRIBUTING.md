# Contributing

The most useful contribution to this project is **a new CHECK shape**, because
every one of them moves a number twenty-four real schemas can verify.

This document is how to add one. It is written against what the corpus actually
contains rather than what seems likely — that distinction is the whole method
here, and it is easy to spend a weekend on a shape no real schema uses.

## The rule everything follows

> Never emit a row that cannot be *shown* to satisfy every constraint that was
> read.

A CHECK constraint is never evaluated and there is no expression parser. There
is a closed set of exact shapes in [src/checks.rs](src/checks.rs), each with a
satisfaction you can point at. Anything outside the set refuses its table by
name and quotes the rule. Widening the set is how reach goes up; guessing is
not.

## Look before you build

Run the survey. It prints every CHECK the closed set does not recognise, across
the whole corpus:

```
python tests/corpus/fetch.py          # once, to get the schemas
cargo test --test corpus -- --ignored --nocapture survey_the_checks
```

One schema at a time, when you are chasing something specific:

```
PGSEED_ONLY=gitlab cargo test --test corpus -- --ignored --nocapture survey_the_checks
```

**Read the output before choosing.** As of the last run there are 2,555 checks,
1,952 understood and 603 not — and the 603 is not a backlog of CHECK shapes:

| count | what it is |
|---:|---|
| 313 | a **trigger**, not a CHECK at all — these are synthesized pseudo-checks, and no expression work touches them |
| 149 | other |
| 67 | an index expression |
| **48** | a **tagged union** — `(type = 'machine' AND col IS NULL) OR (type = 'human')` |
| 18 | a regex `~` |
| 5 | an exclusion constraint |
| 2 | `= ANY ('{a,b}'::text[])`, the array-literal spelling |

The tagged union is the largest genuine CHECK gap and is
[measured and deliberately deferred](docs/plan-corpus-breadth.md) — it is 43
constraints in Zitadel, 4 in Sourcegraph and 1 in GitLab, so it is worth about
six tables. If you want it, that write-up has the reasoning and the counts.

Some shapes that look obviously missing are already handled, and the survey is
how you find that out rather than by reading the source: a range with both
bounds (`x >= 0 AND x <= 100`) works because `interpret_all` splits top-level
`AND` and both bounds exist; `IN` lists work as `ValueSet`; `LIKE` and
`CASE WHEN` do not appear in the corpus at all.

## Adding a shape

1. **Add a `Meaning` variant** in [src/checks.rs](src/checks.rs) describing what
   a satisfying value looks like — not what the expression says.
2. **Match it** in `interpret`, on the exact text Postgres produces for that
   constraint. Postgres normalises: `IN (...)` comes back as `= ANY (...)`, and
   casts are explicit. Match what `pg_get_constraintdef` prints, which the
   survey shows you verbatim.
3. **Teach `classify`** that the shape is satisfiable, in `direct_refusals`.
4. **Teach `generate`** to produce a value that satisfies it, via `Bounds`.
5. **Unit tests** in `checks.rs` for the parse and in `generate.rs` for the
   value. Both are pure and need no database.

## What a shape has to ship with

A measured before and after, against the same corpus:

```
cargo test --test corpus -- --test-threads=1 --nocapture   # the gate
cargo test --test probe  -- --ignored --nocapture          # reach
```

Quote the reach figures from both runs in the pull request. If the number does
not move, that is a fine result and worth saying — two levers in this project
were abandoned exactly that way, and the measurement is why.

**The gate is zero rejections, not a percentage.** If Postgres refuses one
generated row, the change is wrong however much reach it added. That is not a
threshold to tune; it is the argument the whole project rests on.

## Everything else

- `cargo test` — unit tests, no database, no network.
- `cargo test --test oracle --test introspection --test cli --test stress` —
  against a real Postgres that `postgresql_embedded` downloads and starts.
  No Docker, no install.
- `cargo fmt --check` and `cargo clippy --all-targets`. CI runs clippy with
  `-D warnings`, and pins the toolchain in `rust-toolchain.toml` so it lints
  against the same compiler you have.
- The MSRV in `Cargo.toml` is checked by its own CI job. Raising it is a
  decision, not a side effect.

The corpus is fetched, never committed — those schemas belong to their projects.
`tests/corpus/sources.json` pins every one to a commit, so a benchmark run here
and a benchmark run in CI measure the same bytes. Moving a pin is a commit
somebody can review.
