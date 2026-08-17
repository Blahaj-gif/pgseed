# Plan: index expressions, then the nine, then eleven more

Written after reading unique and expression indexes exposed a reach collapse
on GitLab, 86% to 18%. Every number below is measured, and the ones that are
not yet known are named as such rather than estimated.

The order is deliberate. Adding schemas first would measure the tool in a state
already known to be bad, and every schema added is cheap to add and expensive
to re-measure.

---

## Phase 1 — a closed set for index expressions

### What the survey found

86 index expressions across the nine schemas. Clustered by shape:

| n | shape | example |
|---:|---|---|
| **37** | **`lower(col)`, alone or beside plain columns** | `btree (lower((email)::text))`, `btree (namespace_id, lower(name))` |
| 10 | jsonb navigation | `btree ((((data)::jsonb ->> 'display_username'::text)))` |
| 4 | array subscript | `btree ((namespace_traversal_ids[1]), created_at)` |
| 3 | a null test as an expression | `btree (achievement_id, ((revoked_by_id IS NULL)))` |
| 32 | long tail, no shape worth more than 2 | |

`lower()` is 43% of the total on its own, and no other shape is close.

### The insight this rests on

**A non-unique expression index and a unique one ask different questions**, and
conflating them is what makes the current refusal too blunt.

- A **non-unique** expression index enforces nothing. It still rejects rows,
  but only because the expression is *evaluated* on every insert — Discourse's
  `((data)::jsonb ->> 'display_username')` rejects an ordinary word because the
  cast fails, not because anything is duplicated. So the only question is:
  **can this expression fail?**
- A **unique** expression index asks the harder question: **can the expression
  be made distinct?**

`lower(col)` answers both. It is total — no text input makes it fail — so a
non-unique one constrains nothing at all and can be ignored outright. And every
string this generates is already lowercase, so `lower(col)` equals `col`, and
making `col` distinct makes `lower(col)` distinct. That is the same reasoning
the existing `Lowercase` meaning already rests on, and it is provable rather
than probable.

### The design

Translate an index into **the constraints it implies**, where those can be
shown, rather than adding a new kind of rule:

| index | becomes |
|---|---|
| non-unique, every expression total | nothing — it constrains no data |
| `UNIQUE (lower(a))` | a `UniqueKey` over `a`, plus a lowercase bound on `a` |
| `UNIQUE (b, lower(a))` | a `UniqueKey` over `(b, a)`, plus a lowercase bound on `a` |
| anything else | refused, quoting `pg_get_indexdef` |

This needs no new machinery downstream: composite unique keys, `variations`,
and `capacity` all already handle the result. The work is a parser for the
expression list in an index definition, and it belongs in its own module
(`src/indexes.rs`) rather than in `checks`, because it is reading a different
grammar and the two should not learn to be vaguely tolerant of each other.

### Deliberately left out

Each of these is satisfiable in principle. None is in the first set, because
each needs its own argument and a shape admitted without one is exactly the
failure this project is built against.

- **Any cast** — `(x)::jsonb`, `(x)::integer`. These can fail, which is the
  whole reason Discourse rejected a row.
- **jsonb navigation** — total in itself, but always sitting on a cast here.
- **Array subscript** — actually total in Postgres (out of range gives NULL),
  so this is the strongest candidate for the *second* widening.
- **`(col IS NULL)`** — total, and a bounded domain of two. Also a good second
  candidate.

### Pre-registered gates

Written before the work, so they cannot be adjusted to fit the result.

| | |
|---|---|
| **Rejections** | must stay at **zero** across all nine schemas. A widening that recovers reach by writing rows the database refuses has failed, whatever the reach number says. |
| **Reach** | measured, **not predicted**. 37 of 86 expressions are covered, but reach moves non-linearly because refusal is contagious — a handful of these sit on GitLab's central tables and everything downstream follows. Guessing a number here and then hitting it would prove nothing. |
| **Near misses** | every shape left out is tested to *stay* refused, the same as the CHECK closed set. |

---

## Phase 2 — remediation of the nine

The corpus gate was measuring a database less constrained than the real one for
four commits. That specific hole is closed; these are the ones next to it.

### 1. A failed `CREATE INDEX` is not counted as a lost constraint

`lost_constraints` counts failed `ALTER TABLE ... ADD CONSTRAINT` only. Now
that indexes are applied and read, a failed `CREATE UNIQUE INDEX` leaves the
table less constrained than the real one and is invisible — the same shape of
hole as the one just fixed, one level down. **Do this first**; it is small, and
it tells us whether the rest of this phase is even needed.

### 2. Hasura is measuring almost nothing

2 statements applied, **11 skipped**, 2 tables read. It contributes a
denominator of 2 and a reach of 50%, which is noise rather than evidence.
Either point it at a file that carries the real catalogue schema, or drop it
and let one of the eleven take its place. Not worth keeping as it stands.

### 3. Skipped statements, in the three schemas that have any

PostgREST 17 skipped and 1 constraint lost, GitLab 8 and 2, Discourse 8. Small
against 12,191 applied for GitLab, but unexamined. Read what they are; a
skipped `CREATE TABLE` costs a table from the denominator and is harmless,
while a skipped constraint flatters the measurement.

### 4. Turn the ceiling into an assertion

`lost_constraints` is printed and never asserted. Once 1–3 are known, put a
hard upper bound on it per schema, so a change that starts dropping constraints
fails rather than quietly improves the score.

---

## Phase 3 — eleven more schemas

The plan's gate was twenty and there are nine. Nine has been enough to find
every bug so far, which is an argument for the corpus and not against growing
it: each new schema has cost roughly one real bug.

### Confirmed reachable, one file each

Fetched and size-checked, not merely remembered:

| schema | project | licence | size |
|---|---|---|---:|
| lago | getlago/lago-api | AGPL-3.0 | 532 KB |
| sourcegraph | sourcegraph (frontend) | Apache-2.0 | 320 KB |
| sourcegraph_codeintel | sourcegraph | Apache-2.0 | 24 KB |
| sourcegraph_insights | sourcegraph | Apache-2.0 | 34 KB |
| plausible | plausible/analytics | AGPL-3.0 | 78 KB |
| hexpm | hexpm/hexpm | Apache-2.0 | 87 KB |

Six, and they widen the range in ways the current nine do not: Sourcegraph's
three are genuinely separate databases from one product, Plausible and Hexpm
are Ecto rather than Rails or hand-written, and Lago is a modern Rails
`structure.sql` a decade newer than Discourse's lineage.

### The other five need the fetcher to grow

Most modern projects keep migrations as a **directory of numbered files**
rather than one consolidated schema — Mattermost, Vaultwarden, Ory Kratos,
Woodpecker, Zitadel, Immich all do. Both Mattermost and Vaultwarden were
confirmed reachable file-by-file.

So `fetch.py` gains one capability: given a repository path, list it through
the GitHub contents API and concatenate the `.up.sql` files in name order. That
is the whole change, and it unlocks far more than five candidates.

Worth being explicit about the cost: a concatenated migration directory is a
*replay*, so a table created and later altered arrives in its final state only
if every migration applies. The existing harness already tolerates statement
failures and reports them, and item 1 of Phase 2 is what makes that tolerance
honest — which is another reason Phase 2 comes first.

### Licences

Every schema is fetched, never committed; `sources.json` records the URL and
the licence, and `tests/corpus/*.sql` stays in `.gitignore`. Nothing here
redistributes anybody's schema, and the AGPL entries are read by a test rather
than incorporated into anything.

---

## Order, and why

1. **Phase 2, item 1** — the uncounted lost index. It is small and it decides
   whether the rest of Phase 2 matters.
2. **Phase 1** — the closed set. It is where the reach is.
3. **Phase 2, items 2–4** — with Phase 1 done, a re-measure is one run rather
   than several.
4. **Phase 3** — last, and the six confirmed ones before the fetcher change,
   so that a new capability and a new denominator do not land together.
