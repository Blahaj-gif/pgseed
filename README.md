# pgsow

**Point it at a Postgres connection string. It reads the schema and produces
data that satisfies it — or names the table it will not touch, and why.**

```
pgsow --dsn postgres://localhost/mydb            # SQL to stdout
pgsow --dsn ... --apply --truncate               # straight into the database
pgsow --dsn ... --plan                           # say what it would do, write nothing
```

SQL goes to stdout and the report to stderr, so `pgsow --dsn ... > seed.sql`
gives a file that runs. `--apply` writes inside one transaction: all of it, or
none of it.

No codegen step. No generated client. No API key. No runtime — one binary.

### Options

| | |
|---|---|
| `--dsn` / `$DATABASE_URL` | where the schema is read from |
| `--schema NAME` | schemas to read, repeatable; default `public` |
| `--rows N` | rows per table; default 50 |
| `--rows TABLE=N` | override one, repeatable. Takes `*` and `?`; last match wins |
| `--include` / `--exclude` | which tables to touch. `--exclude` wins on a conflict |
| `--seed S` | same seed and schema give byte-identical SQL |
| `--out FILE` | write the SQL here instead of stdout |
| `--apply` | write the rows, in one transaction |
| `--truncate` | empty the targets first, in dependency order. Never CASCADE |
| `--allow-nonempty` | write even though the targets already hold rows |
| `--remote` | write to a database that is not on this machine |

The last two are the only guards, and both are questions rather than guesses.
Reading a database name for the word `prod` stops nobody who called theirs
`main` and annoys everybody whose local copy is called `myapp_production_dump`.
So instead: **is the host this machine**, and **do the target tables already
hold rows** — two facts a person can answer instantly and a tool cannot.

## Reach, on eighteen schemas nobody here wrote

| schema | tables | fillable | reach |
|---|---:|---:|---:|
| PowerDNS | 7 | 7 | 100% |
| Hasura | 8 | 3 | 38% |
| Kong | 9 | 8 | 89% |
| Sourcegraph *(codeintel)* | 13 | 9 | 69% |
| Harbor | 21 | 21 | 100% |
| Ory Kratos *(replayed migrations)* | 23 | 23 | 100% |
| Sourcegraph *(insights)* | 21 | 17 | 81% |
| hex.pm | 36 | 22 | 61% |
| Vaultwarden *(replayed migrations)* | 29 | 29 | 100% |
| Temporal | 37 | 36 | 97% |
| Plausible | 41 | 18 | 44% |
| Mattermost *(replayed migrations)* | 80 | 75 | 94% |
| PostgREST *(test fixtures — deliberately awkward)* | 132 | 129 | 98% |
| Synapse | 134 | 126 | 94% |
| Lago | 137 | 84 | 61% |
| Sourcegraph *(frontend)* | 180 | 74 | 41% |
| Discourse | 351 | 324 | 92% |
| GitLab | 956 | 412 | 43% |
| **total** | **2,215** | **1,417** | **64%** |

Fetched with `python tests/corpus/fetch.py`; sources and licences in
`tests/corpus/sources.json`. Three are *replayed* migration directories rather
than a snapshot of a finished schema, which only reaches the real shape if
every migration in it applies — so the harness counts the constraints a failed
one costs, and each schema has a ceiling on that which a regression trips. The corpus is deliberately not written here — a
hand-made one would only contain the constructs its author remembered to
handle, which measures agreement rather than accuracy.

## Status

It introspects, classifies, and **generates**. Point it at a database and it
writes SQL; `--plan` reports what it would do and writes nothing.

Every row it produces is checked by the only authority that matters: a test
generates for each corpus schema at the default fifty rows, applies the result
to a real Postgres, and fails if a single statement is rejected. It currently
generates **1,830 statements across the eighteen schemas and Postgres accepts
every one** — with every one of their 456 triggers installed. The gate is zero, not a percentage — the whole thesis is that the
database adjudicates, so one row it refuses is a failure rather than a figure
to be pleased with.

```
pgsow: 14 tables, 12 fillable, 2 refused (86% reach)

  would fill, in this order:
    users
    orders
    order_items

  refused:
    invoices     CHECK "invoices_total_positive" CHECK ((total > (0)::numeric))
                 — this cannot prove a generated row satisfies it, so it will
                 not write one
    shipments    foreign key cycle between shipments and orders — every key in
                 it is NOT NULL and not deferrable, so no order of single-row
                 inserts can satisfy it. Making shipments_order_fk deferrable,
                 or one of its columns nullable, would be enough.
```

## The rule

**Never emit a row that cannot be shown to satisfy every constraint that was
read.**

The failure mode of a seed tool is not that it crashes. It is that it inserts
plausible rows which quietly violate a rule nobody re-checked, and everything
downstream is then tested against data the real system would have rejected.

So a CHECK constraint is **never evaluated**. There is no expression parser,
no simplification and no reasoning about operators. A partial solver that
silently handles `total > 0` but mishandles `total > 0 OR status = 'void'` is
worse than no solver at all, because the cases it gets wrong look exactly like
the cases it gets right.

What there is instead is a **closed set of exact shapes**, each with a
satisfaction that can be pointed at. A definition matches one of them
structurally or it is unknown and its table is refused. The set was widened
once, from a survey of all 277 constraints in the corpus that it did not
understand — evidence rather than guesswork — and every near miss is tested to
make sure it *stays* unknown. `num_nonnulls(a, b) <= 1` is satisfied by nulling
both and `> 0` by filling both; both are perfectly satisfiable, both are
different obligations, and neither is folded into `= 1` on the grounds of
looking similar.

| shape | how it is satisfied |
|---|---|
| `char_length(col) <= N` | what `varchar(N)` already means |
| `col IS NOT NULL` | a column this never writes NULL into |
| `octet_length(col) = N` / `<= N` | a fixed width, or a ceiling |
| `col > N`, `col >= N` | a floor on the generated number |
| `col = lower(col)` | every string generated is lowercase |
| `col IS NULL OR …` | writing NULL satisfies the whole disjunction |
| `num_nonnulls(a, b[, c]) = 1` | fill one, NULL the rest |
| `jsonb_typeof(col) = 'object'` | emit a value of that type |
| `cardinality(col) <= N` | every array written holds one element |

Exclusion constraints are read as the checks they are and refuse. Not seeing a
constraint is not the same as satisfying it.

Three outcomes, never two: **filled · refused · could-not-read.**

Exit codes follow: `0` everything fillable, `1` something refused, `2` the
schema could not be read — so it composes in a script without anybody parsing
prose.

## What it handles today

| | |
|---|---|
| Insert order | topological, ties broken by name so runs are reproducible |
| FK cycles | broken through a nullable key, or by deferring a deferrable one |
| Unbreakable cycles | refused, naming the constraint that would have to change |
| Composite keys | referencing and referenced columns kept in declared order |
| Types | integers, numerics with precision, text with declared length, uuid, dates, timestamps, json, bytea, enums with their labels, domains, arrays |
| Generated & identity columns | left out of the insert entirely, not defaulted |
| Refusal contagion | a table whose required key points at a refused, or unread, table is refused too, all the way down |
| Row counts | capped at what a table can hold — a `bool UNIQUE` holds two rows, a join table holds as many as it has pairs, and a cap on a parent caps its children |
| Composite unique keys | one column varies, or where none has room to spare the columns are walked as digits of one number |

## Why this exists

The category has proven demand and no maintained free option. `neosync` is
archived. `supabase-community/seed` has 788 stars, 25 open issues including
unpatched high-severity advisories, and its last real commit was August 2024 —
it also requires a codegen step and a generated TypeScript client.

Determinism is *not* the differentiator; that tool has it too. The
differentiators are: no codegen, no client, no runtime, no API key, and being
alive.

## Speed

Measured on a **release** build, which is what ships. A debug test binary is
about seven times slower, and quoting one of those was how this README once
claimed 68 seconds for a job that takes ten.

| | generate | SQL |
|---|---:|---:|
| 14 tables, 50 rows each | 28 ms | 63 KB |
| GitLab, 50 rows each | 0.3 s | 5 MB |
| GitLab, 1000 rows each | 4.9 s | 103 MB |

The SQL is streamed a statement at a time rather than built and then written,
so the last row of that table is a file size and not a memory requirement.

## Testing

```
cargo test                         unit tests, no database, no network
cargo test --features integration  against a real Postgres
```

Integration tests run a **real Postgres**, downloaded and started per test by
`postgresql_embedded` — no Docker, no install. That matters more here than in
most projects: the entire correctness argument is that the database
adjudicates, so the tests need a database rather than a model of one.

The schema corpus is **not written by this project**. It is real schemas taken
from open-source projects' migrations, recorded as a manifest of sources. A
hand-written corpus would only contain the constructs its author remembered to
handle, which measures agreement rather than accuracy.

## Licence

MIT.
