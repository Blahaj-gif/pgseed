# Plan: the blind spots, the outliers, the time, and twenty schemas

Written after measuring each of the four rather than from the impressions in
the last review. One of the four turned out to be largely a measurement error
of mine, which is recorded here rather than quietly dropped.

---

## 1. The blind spots: triggers and partition routing

### What is actually there

Counted across the corpus, not estimated:

| | schemas affected | count |
|---|---:|---:|
| `CREATE TRIGGER` | 9 of 18 | **456** (387 GitLab, 29 Sourcegraph, 15 PostgREST) |
| `PARTITION BY` | 3 of 18 | **106** (101 GitLab) |

Neither is installed by the corpus gate, so neither can reject a row *in the
gate*, and both do in reality. The `volume` benchmark applies the whole dump
and hits both immediately:

```
P0001  Discourse: require_reply_approval in category_settings is readonly
23514  no partition of relation "merge_request_diff_commits_b5377a7a34" found for row
```

### The two halves, which must not be confused

**Gate fidelity** and **tool behaviour** are separate problems, and only the
first is cheap.

**1a — install them in the gate.** Let `CREATE TRIGGER` and the partition DDL
through the head filter. This makes the gate measure the real database and will
*lower* the numbers, which is the point: the last two times this project found
the gate measuring something less constrained than reality, the honest number
was worse and the flattering one had been quoted for weeks.

Do this first, alone, and record what it costs. It is a filter change.

**1b — decide what the tool does about a trigger.** The hard half, and the
index work says how to approach it: **a trigger only matters if it can raise.**
Most of the 456 maintain an `updated_at` or denormalise a counter and cannot
refuse anything.

Proving a `plpgsql` body cannot raise means reading it, and reading it is the
expression evaluator this project does not have. So, in order:

- **Refuse any table with a row-level INSERT trigger.** Correct, blunt, and
  measurable in an hour. Establishes the floor.
- **A closed set over the trigger body**: no `RAISE`, no `ASSERT`, no call to
  another function, and the body a single `NEW.x := ...` or `RETURN NEW`.
  Anything else refuses. Same shape as `checks` and `indexes`, and survey
  first, because the top shapes among 456 triggers are probably three idioms.

**1c — partition routing.** A partitioned parent is not read at all today
(`relkind = 'r'` excludes `'p'`), so its children are refused for pointing at
something unread. Reading them requires knowing a row will land somewhere, and
that is legible: `pg_get_expr(relpartbound)` gives `FOR VALUES FROM (1) TO
(100)` or `FOR VALUES IN (...)`. A closed set of two bound shapes — range and
list — feeding a bound into `generate` covers almost all real use, and a
`DEFAULT` partition makes it trivial where one exists.

Worth doing on its own merits: 101 invisible tables in GitLab, each of which
unblocks its children.

### Gate

Rejections stay at zero **after** 1a lands. If installing triggers makes rows
fail, the tool is wrong and 1b is not optional.

---

## 2. Sourcegraph frontend, at 44%

31 tables directly refused by 48 rules, and the profile is nothing like
GitLab's:

| n | shape | prospect |
|---:|---|---|
| 10 | a regular expression match | **none** — matching a regex needs an engine, and generating a string that satisfies one is the solver this refuses to build |
| 7 | multi-branch boolean over three or more columns | poor — each is genuinely different |
| 4 | `col = ANY (ARRAY[...])` | **good** — a value set is an enum by another spelling |
| 3 | `CASE WHEN ... THEN ... END` | fair, as a closed shape |

**Regex is the honest wall.** 18 across the corpus, 10 of them here, and no
amount of closed-set work touches them. That is a permanent floor, and saying
so beats implying the number will keep climbing.

`col = ANY (...)` is the one clear win, and it is worth doing for the corpus as
a whole rather than for Sourcegraph: 6 CHECKs plus 2 index expressions, and the
last shape with an obvious satisfaction.

**Expect 44% to reach the mid-fifties and stop.**

---

## 3. The time — a correction first

**The review said 68 seconds to generate GitLab at 1000 rows a table. That was
a debug build.** In release:

| | debug | release |
|---|---:|---:|
| GitLab @ 1000 rows | 68.6 s | **9.7 s** |
| Discourse @ 1000 rows | 29.4 s | **2.5 s** |

Seven times faster, and the tool ships as a release binary. The number in the
review was the test harness measuring itself.

What is left is much smaller than it looked:

- **9.7 s for 712,000 rows** is about 13 microseconds a row. Not a problem
  anybody has.
- **33% of a cell is the per-cell RNG** — measured: 200,000 cells cost 94 ms,
  of which seeding is 31 ms. `ChaCha8` was chosen for reproducibility across
  machines and releases, which a splitmix-class generator gives just as firmly
  and far more cheaply. Worth perhaps 9.7 s down to 7 s. **Not worth doing
  now**: it changes every generated value, to buy speed nobody is short of.
- **205 MB held in one `String`** is the real remaining sharp edge, and it is
  memory rather than time. `emit::sql` builds the whole thing before anything
  is written. Streaming statement by statement would cap memory at one
  statement, and `--out` already knows where it is going.

**Recommendation:** fix the memory, correct the README, leave the RNG, and note
release mode on the speed gate so this cannot be misread twice.

---

## 4. Twenty schemas, with coverage, coherency and validation

Eighteen are fetched and one more is configured. Reaching twenty is the easy
part; the three words after it are what is worth planning.

### Getting to twenty

- **Ory Hydra** is configured and unfetched: the contents API allows sixty
  calls an hour without a token, and finding the candidates spent them. One run
  with `GITHUB_TOKEN` set. CI already sets it.
- **One more.** Fourteen candidates were checked and most keep migrations as
  Go, Python or XML. Budget an hour of looking rather than assuming one is
  there to be found.

### Coverage — say what the corpus spans, and where it is thin

The corpus is a list. It should be a **matrix**, checked by a test, so that
"eighteen schemas" stops being the claim and "these constructs, in real
schemas" starts being it. One row per construct:

```
                      schemas covering it
composite primary key        11
foreign key cycle             6
partitioned table             3      <- thin, and blind (see 1c)
domain type                   2      <- thin
exclusion constraint          1      <- thin
enum array                    0      <- absent; tested only by hand
```

The value is in the last column. A construct with one schema behind it is one
schema away from being untested; a construct with none is tested only by DDL
this project wrote, which is the mistake the corpus exists to avoid. **Write
the matrix, then add schemas to fill its holes** rather than to reach a round
number.

### Coherency — one shape for a schema entry

Three shapes exist: a single URL, a directory replay, and a directory replay
with exclusions. `sources.json` carries `kind`, `suffix`, `nested` and
`exclude` fields that apply to some entries and not others, and the corpus test
carries a parallel list of names and ceilings kept in step by hand. Two lists
that must agree is one too many.

**Fold the ceiling into `sources.json`** and have the test read it, so a schema
is described in exactly one place.

### Validation — what a new schema must prove before it counts

A schema added today is trusted on the strength of the suite going green. It
should clear a stated bar, asserted per schema:

1. **It loaded.** A floor on statements applied, so a schema that silently
   half-loads cannot sit in the list looking like evidence. Hasura sat at two
   tables of eight for weeks.
2. **It is not under-constrained.** Done — the lost-constraint ceiling.
3. **Zero rejections.** Done.
4. **It contributes something.** A schema whose constructs are all covered
   elsewhere adds runtime and no information. Not a hard gate, but the matrix
   makes it visible.

---

## Order

1. **1a, the gate filter.** Cheapest, and everything else is measured against
   whatever it reveals. It may make the rest of this plan wrong, which is the
   best reason to do it first.
2. **The memory fix and the release-mode correction.** Small, and stops a wrong
   number being repeated.
3. **`col = ANY (...)`.** The last shape with an obvious satisfaction.
4. **1c, partition bounds.** The largest remaining structural win — 101 tables
   in GitLab alone.
5. **1b, triggers.** Floor first, closed set after a survey.
6. **Twenty schemas, the matrix, and one shape for a schema entry.**
