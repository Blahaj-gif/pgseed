# pgseed

Point it at a Postgres database. It reads the schema and writes data that fits
— or tells you which table it will not touch, and which constraint stopped it.

```
pgseed --dsn postgres://localhost/mydb --apply     # fill the database
pgseed --dsn postgres://localhost/mydb > seed.sql  # or write a file
pgseed --dsn postgres://localhost/mydb --plan      # or just say what it would do
```

No config file. No codegen step. No generated client. No API key. One binary.

```sql
INSERT INTO "public"."users" ("email", "username", "first_name", "last_name", "timezone") VALUES
  ('ada.achebe@example.com',    'ada.achebe',    'Ada',   'Achebe',  'Europe/Berlin'),
  ('amara.adeyemi@example.org', 'amara.adeyemi', 'Amara', 'Adeyemi', 'America/Chicago');
```

The way a seed tool goes wrong is not by crashing. It is by writing plausible
rows that break a rule nobody re-checks, so everything downstream is tested
against data the real system would reject. This one never writes a row it
cannot show satisfies every constraint it read, and says so when it cannot.

![pgseed --plan listing the tables it will fill and naming the one it refuses,
with the constraint quoted](docs/media/plan.svg)

That is real output, against Sourcegraph's codeintel schema. The refused table
is named, the rule is quoted, and nothing is written into it.

Across 24 open-source schemas — 2,586 tables — Postgres accepted every row it
generated. It fills 67% of those tables by reasoning alone, and 88% when it is
allowed to ask the database. Where those come from and what they exclude is in
[How well it works](#how-well-it-works).

## Install

Download a binary from the [releases page](https://github.com/Blahaj-gif/pgseed/releases)
— Linux, macOS and Windows, each built and started by CI before it is
published. Or build it yourself:

```
cargo install --git https://github.com/Blahaj-gif/pgseed
```

Nothing else is needed: no Docker, no Node, no daemon, no account.

## Getting started

**Guard a schema in CI.** `--plan` writes nothing and exits 1 if any table was
refused, so a migration that adds a constraint nothing can satisfy turns the
build red before it reaches anyone's dev database. This is the one that runs
without being remembered:

```yaml
- run: pgseed --dsn "$DATABASE_URL" --plan
  env:
    DATABASE_URL: postgres://postgres:postgres@localhost:5432/postgres
```

Pair it with a `postgres` service container and your migration step. Nothing to
install beyond the binary, and no state left behind — `--plan` only reads.

**Make a seed file to commit.** The same seed and schema give byte-identical
SQL, so a committed file is a golden fixture: every branch and every test run
starts from exactly the same rows, and a change to the data shows up as a diff
somebody can read.

```
pgseed --dsn "$DATABASE_URL" --rows 200 > seed.sql
psql "$DATABASE_URL" -f seed.sql
```

**Fill a development database.** Wipes the target tables first, then writes 50
rows each, inside one transaction:

```
pgseed --dsn "$DATABASE_URL" --apply --truncate
```

**Fill more of the database.** With `--probe`, every table pgseed refused is
offered to the database behind a savepoint and kept if it is accepted. Reach
goes from 67% to 88% on real schemas:

```
pgseed --dsn "$DATABASE_URL" --apply --truncate --probe
```

**Point it at part of a schema.** Patterns take `*` and `?`, and `--exclude`
wins over `--include`:

```
pgseed --dsn "$DATABASE_URL" --apply \
      --include 'billing_*' --exclude '*_audit' --rows 20 --rows 'invoices=500'
```

## Options

| | |
|---|---|
| `--dsn` / `$DATABASE_URL` | where to read the schema from |
| `--schema NAME` | schemas to read, repeatable; default `public` |
| `--rows N` | rows per table; default 50 |
| `--rows TABLE=N` | override one table, repeatable; takes `*` and `?`, last match wins |
| `--include` / `--exclude` | which tables to touch; `--exclude` wins on a conflict |
| `--seed S` | the same seed and schema give byte-identical SQL |
| `--out FILE` | write the SQL here instead of stdout |
| `--apply` | write the rows, in one transaction |
| `--truncate` | empty the targets first, in dependency order; never CASCADE |
| `--allow-nonempty` | write even though the targets already hold rows |
| `--probe` | offer the refused tables to the database, keep what it accepts |
| `--remote` | write to a database that is not on this machine |

Exit codes: **0** everything filled · **1** something refused · **2** the schema
could not be read.

Two guards, and both are questions rather than guesses: **is the host this
machine**, and **do the target tables already hold rows**. Reading a database
name for the word `prod` stops nobody who called theirs `main`, and annoys
everybody whose local copy is called `myapp_production_dump`.

## What it refuses, and why

Most seed tools produce rows. This one tells you when it cannot.

> **Never write a row that cannot be shown to satisfy every constraint that was
> read.**

A table with a rule pgseed cannot prove it satisfies is named, the rule is
quoted, and the table is left alone:

```
pgseed: 14 tables, 12 fillable, 2 refused (86% reach)

  refused:
    invoices     CHECK "invoices_total_positive" CHECK ((total > (0)::numeric))
                 — this cannot prove a generated row satisfies it, so it will
                 not write one
    shipments    foreign key cycle between shipments and orders — every key in
                 it is NOT NULL and not deferrable, so no order of single-row
                 inserts can satisfy it. Making shipments_order_fk deferrable,
                 or one of its columns nullable, would be enough.
```

A CHECK constraint is never evaluated. There is no expression parser and no
guessing. There is a **closed set of exact shapes**, each with a satisfaction
you can point at — a length limit, a value set, exactly-one-of-these-columns —
and anything outside it refuses. Details and the full list:
[docs/](docs/).

## How well it works

Twenty-four schemas taken from real open-source projects — 2,586 tables —
generated for, applied to a real Postgres, and **not one row rejected**. The
gate is zero, not a percentage, because the database is the judge.

|  | by reasoning | with `--probe` |
|---|---:|---:|
| of every table in the corpus | 66.6% | **88.1%** |
| of the tables that can hold a row | 69.3% | **91.6%** |
| averaged per schema, unweighted | 82.8% | **94.3%** |

Three numbers because they answer three questions, and the gap between the
first and the last is worth being blunt about: **GitLab is 1,057 tables, 41% of
the entire corpus, and scores 40%.** The corpus-wide figure is therefore
substantially a report on how this handles GitLab. Without it the same corpus
reads 84.9% and 95.8%.

Neither framing is the honest one on its own. Counting tables is the harder
test and the one that judges the tool, so it stays the headline. The per-schema
average is closer to what a reader's own schema will do, since most schemas are
not GitLab — 21 of the 24 are at 88% or better with `--probe`. The spread
between them is a fact about the corpus, not a choice about presentation.

The second row excludes
**100 tables that can hold no row from anybody**: 83 partitioned tables whose
dumps declare the parent and create the partitions at runtime, and 17 more
downstream of one.

The schemas are chosen for spread rather than for count. Rails, Ecto, Prisma,
Diesel, Java/MyBatis, Go migrations and hand-written SQL are all here, because
every generator emits differently shaped DDL and a corpus of one ecosystem
measures agreement with that ecosystem rather than accuracy.

<details>
<summary>Per schema</summary>

| schema | tables | by reasoning | with `--probe` |
|---|---:|---:|---:|
| PowerDNS | 7 | 100% | 100% |
| Hasura | 8 | 88% | 88% |
| Kong | 9 | 100% | 100% |
| Sourcegraph *(codeintel)* | 13 | 77% | 92% |
| listmonk | 16 | 100% | 100% |
| Ory Hydra | 19 | 84% | 100% |
| Harbor | 21 | 100% | 100% |
| Sourcegraph *(insights)* | 21 | 81% | 95% |
| Vaultwarden | 29 | 100% | 100% |
| Zitadel *(v3)* | 30 | 17% | 37% |
| Ory Kratos | 31 | 94% | 97% |
| hex.pm | 36 | 61% | 100% |
| Temporal | 37 | 100% | 100% |
| Plausible | 41 | 44% | 100% |
| Camunda 7 | 49 | 98% | 100% |
| Documenso | 61 | 100% | 100% |
| Langfuse | 81 | 91% | 100% |
| Mattermost | 82 | 94% | 100% |
| Synapse | 134 | 99% | 99% |
| PostgREST *(test fixtures)* | 134 | 96% | 98% |
| Lago | 138 | 83% | 98% |
| Sourcegraph *(frontend)* | 180 | 43% | 83% |
| Discourse | 352 | 97% | 99% |
| GitLab | 1,057 | 40% | 77% |
| **total** | **2,586** | **66.6%** | **88.1%** |

Measured twice on separate runs, identical both times, and identical again on
Linux in CI — same table counts, same reach, every schema. Each one is pinned
to a commit, so the numbers are reproducible rather than whatever a branch
pointed at that day.

</details>

The corpus is **not written by this project**. A hand-written one only contains
the constructs its author remembered to handle, which measures agreement rather
than accuracy. Sources and licences are in `tests/corpus/sources.json`; fetch
them with `python tests/corpus/fetch.py`.

## Speed

Release build, which is what ships, timed on the streaming path `--out` uses.

| | generate | SQL |
|---|---:|---:|
| 14 tables, 50 rows each | 9 ms | 66 KB |
| Discourse, 100 rows each | 0.9 s | 3 MB |
| GitLab, 50 rows each | 0.7 s | 6 MB |
| GitLab, 1000 rows each | 16 s | 125 MB |

The SQL is streamed one statement at a time, so a large schema at a large row
count is a question of patience rather than of memory.

These come from `cargo test --release --test corpus -- --ignored readme_speed`,
so they are numbers anybody can reproduce rather than a timing taken once. With
`--apply` the cost is the database's rather than this tool's: writing
Discourse's 343 statements takes about 2.5 s on top of generating them.

## What the data looks like

Column names are read the same way constraints are: a closed set of exact
shapes, matched on the whole name, its last two segments, or its last one.
`email` gets an address, `country_code` gets `SE`, `checksum` gets 64 hex
characters, `quantity` gets a number between 1 and 20. A camel hump counts as
a word boundary, so Prisma's `twoFactorSecret` reaches the same noun as
everybody else's `two_factor_secret`.

- **Columns in one row describe the same person.** `first_name`, `last_name`
  and `email` are three readings of one number, so they agree by construction.
- **Rows in a child table describe the parent they point at.** Fill 3 users and
  7 notes and every note names the user it references.
- **Timestamps run forwards.** `created_at` lands at or before `updated_at`,
  both before `deleted_at` or `expires_at`, and a child row is not created
  before the row it points at. Where the column has a database default — most
  Prisma and Ecto schemas — the database fills it and none of this applies.
  Postgres accepts any ordering, so nothing was ever rejected for this: the
  rows were valid and the application logic on top of them was not, which is
  the failure this opens by describing.
- **Nothing generated can reach anybody.** Addresses use the RFC 2606 reserved
  domains, phone numbers the 555-01xx block, and IPs the RFC 5737 documentation
  ranges — or `10.0.0.0/8` for an `inet` or `cidr` column, which needs more
  addresses than those hold. Neither routes anywhere.
- **Unique columns stay unique.** Where being distinct and being coherent
  conflict, distinct wins — seven email addresses cannot come from three
  people.

Which names to cover was counted, not guessed: 6,901 text columns across the
corpus, of which 79% land on a known noun. The rest get an ordinary word and no
claim.

## Testing

```
cargo test                   unit tests, no database, no network
cargo test -- --ignored      the surveys: coverage, triggers, partitions, reach
```

Integration tests run a **real Postgres**, downloaded and started per test by
`postgresql_embedded` — no Docker, no install. That matters more here than in
most projects: the whole correctness argument is that the database decides, so
the tests need a database rather than a model of one.

## Why this exists

The category has proven demand and no maintained free option. `neosync` is
archived. `supabase-community/seed` has 788 stars, 25 open issues including
unpatched high-severity advisories, and its last real commit was August 2024;
it also needs a codegen step and a generated TypeScript client.

Determinism is not the difference — that tool has it too. The difference is: no
codegen, no client, no runtime, no API key, and a correctness claim the
database checks.

## Licence

MIT.
