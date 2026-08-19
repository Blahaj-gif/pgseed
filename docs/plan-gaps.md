# Plan: the four gaps left after publishing

Written from measurement rather than from the impressions in the last review,
and one of the four turned out to be described wrongly in that review. That is
recorded here rather than quietly corrected.

Ordered by *cost to a reader of the output*, not by size.

---

## 1. hex.pm cannot be measured on Linux — a shim, not an excuse

### What is actually there

`postgresql_embedded` ships a Postgres built by `theseus-rs/postgresql-binaries`.
On Windows that build has `uuid-ossp`; on Linux it does not — the extension's
control file is present and its library is missing, so `CREATE EXTENSION` fails
with `could not load library .../lib/uuid-ossp`.

hex.pm defaults a primary key to `uuid_generate_v4()`, so its `CREATE TABLE
users` fails and the twenty constraints on that table are counted as lost. The
gate reports `NOT MEASURED HERE`, which is honest and is not the same as
useful: the corpus is twenty schemas here and nineteen on a runner, and two
numbers that differ by environment are two numbers.

Counted across the corpus, this is the *only* environment difference:

```text
  fails on Windows   vector, pg_partman      (genuinely third-party)
  fails on Linux     vector, pg_partman, uuid-ossp
```

And what the corpus needs from `uuid-ossp` is one function, four times, in
hex.pm and Hydra, every one of them inside a `DEFAULT`:

```text
  4  uuid_generate_v4
```

### The remediation

**Provide the function rather than the extension.** `gen_random_uuid()` has
been in core since Postgres 13, returns the same type, and produces the same
class of value:

```sql
CREATE OR REPLACE FUNCTION uuid_generate_v4() RETURNS uuid
    LANGUAGE sql AS 'SELECT gen_random_uuid()';
```

Installed by the corpus harness *only when the extension failed*, and logged
when it happens, so nothing is silently substituted.

This is worth being careful about, because "make the schema load" is exactly
the kind of pressure that produces a flattering measurement. Three things keep
it honest:

- It changes **no constraint**. Not a CHECK, not a key, not a nullability. It
  supplies a function that a `DEFAULT` calls, and the generator never writes
  defaulted columns anyway — the database evaluates it either way.
- It is **exact**, not approximate. A v4 UUID from either function is a random
  128-bit value with the version bits set.
- It applies to **one named function**. Not "retry whatever failed": a shim is
  written by hand for a function whose semantics are known, or there is no
  shim and the schema stays unmeasured.

`vector` and `pg_partman` get no shim and never will. They are real extensions
with real behaviour, and a schema that needs one is a schema this cannot
measure without it.

**Cost:** an hour. **Effect:** twenty schemas on every platform, and one
number instead of two.

---

## 2. GitLab at 40% — and it is triggers, not partitions

### What is actually there

462 root refusals, counted rather than guessed:

| n | cause |
|---:|---|
| **285** | a trigger that might interfere |
| 87 | a partitioned table (45 range, 42 list) |
| 43 | a CHECK outside the closed set |
| 20 | a non-unique index whose expression might fail |
| 17 | unique keys that contend |
| 1 | a column type with no generator |

Probing rescues 391 of the 634 refused, which is the useful half of the answer:
**most of these refusals are caution rather than reasoning**, and the database
overrules them.

### The remediation, and the measurement it needs first

261 of the 285 triggers are the same shape — `SELECT ... INTO NEW.col FROM
parent WHERE ...`, a sharding key backfilled from a lookup. Today any
assignment to a column this writes counts as interference.

That is stricter than it needs to be. **The assignment only matters if
something was relying on the value.** A trigger that overwrites a nullable
column carrying no constraint has changed nothing this promised. The rule
could narrow to: *interference is an assignment to a column that is NOT NULL,
or under a unique key, or named by a CHECK this claimed to satisfy* — because
`SELECT INTO` can yield NULL, and NULL into any of those three is a rejected
row, while NULL into an unconstrained nullable column is a Tuesday.

**Measure before building.** Count how many of the 285 assign only to columns
that are nullable, keyless and unconstrained. If that number is small, the
lever is not there and this should be written off rather than attempted — the
partition work looked like the biggest lever in the corpus and turned out to be
worth three tables.

**Cost:** a day to measure, two to build if the measurement justifies it.

---

## 3. The partitioned tables that can take no row from anybody

83 of the corpus's 106 partitioned parents have **no partitions at all**. The
dump declares the parent; the application creates partitions at runtime. 90
tables fail with `no partition of relation ... found for row` even after the
database has been asked, and most of the 133 not-null violations behind them
are their children.

Nothing in this tool moves that. A partitioned table with no partitions cannot
hold a row, and reading range bounds — the obvious remediation — is worth
**three tables** across the whole corpus, because only three range-partitioned
parents have partitions and are unread.

### The remediation is a sentence, not a feature

Report **two denominators**, both labelled:

```text
  87.7%  of every table in the corpus
  91.3%  of the tables that can hold a row at all
```

The second is not a flattering restatement of the first as long as both are
printed and the difference is named. It is the more useful number for somebody
deciding whether to run this, and the first is the more useful one for judging
the tool. Publishing only the second would be the flattery; publishing only the
first hides that a twelfth of the corpus is unfillable by anyone.

**Do not** drop those tables from the denominator. **Do** add the second row,
and a `--plan` line naming partitioned tables with no partitions, since a user
staring at a refusal deserves to know it is not about them.

**Cost:** an afternoon, most of it wording.

---

## 4. Identity across tables — the review had this wrong

### What the last review said

> a user's row and their `user_emails` row describe different people

### What is actually true

They agree, until they don't, and the boundary is exact. A child row takes its
identity from **its own row index**; its foreign key points at parent row
`(row / stride) % parent_count`. Those are the same number while the child's
index is below the parent's count, and diverge the moment it wraps:

```text
  --rows 4                          --rows 3 --rows user_emails=7

  Ada Adeyemi   ada.adeyemi@        Ada Adeyemi    ada.adeyemi@
  Amara Castellan amara.castellan@  Amara Castellan amara.castellan@
  Anton Duarte  anton.duarte@       Anton Duarte   anton.duarte@
  Bea Fairbairn bea.fairbairn@      Ada Adeyemi    bea.fairbairn@     <-- wrapped
                                    Amara Castellan callum.ghosh@
                                    Anton Duarte   camila.ibarra@
                                    Ada Adeyemi    dara.jimenez@
```

So the default case is coherent and the flaw shows up exactly when a child is
asked for more rows than its parent has — which is the ordinary shape of a
join table, an events table, or anything with a `--rows` override on it.

### The remediation

**Take the identity from the parent row this row actually points at.** The
emitter already computes that index to resolve the key; it is
`(row / stride) % parent_rows.len()` in the pool path and `row % count` in the
subquery path. Passing it to `value` as the identity, instead of `row`, makes
`first_name`, `email` and `display_name` describe the parent they are attached
to.

Three constraints on the design:

- **The RNG key does not change.** Identity and the per-cell stream are already
  separate arguments; only the identity moves. Every property in
  `generate`'s tests — same seed, same bytes; a neighbouring table's row count
  changing nothing here — is untouched.
- **Distinctness is unaffected.** A column under a unique key is driven by its
  `step`, not by the identity, so borrowing a parent's identity cannot make two
  rows collide.
- **Only where there is one answer.** A table with a single foreign key has an
  obvious parent; one with several does not, and guessing which supplies the
  identity is the sort of cleverness that produces a wrong row that looks
  right. With more than one, keep the row index and say so.

**Cost:** a day, including a test that fills a child deeper than its parent and
asserts every child names the person it points at.

---

## Order

1. **The uuid-ossp shim.** Cheapest, and it is the difference between one
   benchmark and two.
2. **The second denominator.** An afternoon, and it makes the honest number
   legible instead of merely honest.
3. **Identity across the foreign key.** The most visible thing left in the
   output, and the review had it wrong in a direction that made it look
   unfixable.
4. **The trigger narrowing — measured first, and abandoned if the count is
   small.** 285 refusals is the largest single number in the corpus and that is
   exactly why it deserves a measurement before a day of work rather than after.
