# pgsow

Point it at a Postgres database. It reads the schema and writes data that fits
— or tells you which table it will not touch, and which constraint stopped it.

```
pgsow --dsn postgres://localhost/mydb --apply     # fill the database
pgsow --dsn postgres://localhost/mydb > seed.sql  # or write a file
pgsow --dsn postgres://localhost/mydb --plan      # or just say what it would do
```

No config file. No codegen step. No generated client. No API key. One binary.

```sql
INSERT INTO "public"."users" ("email", "username", "first_name", "last_name", "timezone") VALUES
  ('ada.achebe@example.com',    'ada.achebe',    'Ada',   'Achebe',  'Europe/Berlin'),
  ('amara.adeyemi@example.org', 'amara.adeyemi', 'Amara', 'Adeyemi', 'America/Chicago');
```

## Install

```
cargo install pgsow
```

Or download a binary from the [releases page](https://github.com/Blahaj-gif/pgsow/releases).
Nothing else is needed: no Docker, no Node, no daemon, no account.

## Getting started

**Fill a development database.** Wipes the target tables first, then writes 50
rows each, inside one transaction:

```
pgsow --dsn "$DATABASE_URL" --apply --truncate
```

**Make a seed file to commit.** SQL goes to stdout, the report to stderr, so
the file contains only SQL:

```
pgsow --dsn "$DATABASE_URL" --rows 200 > seed.sql
psql "$DATABASE_URL" -f seed.sql
```

The same seed always produces the same bytes, so this diffs cleanly.

**Fill more of the database.** With `--probe`, every table pgsow refused is
offered to the database behind a savepoint and kept if it is accepted. Reach
goes from 65% to 88% on real schemas:

```
pgsow --dsn "$DATABASE_URL" --apply --truncate --probe
```

**Check a schema in CI.** `--plan` writes nothing and exits 1 if anything was
refused, so a new constraint nobody can satisfy fails the build:

```
pgsow --dsn "$DATABASE_URL" --plan || echo "something is unfillable"
```

**Point it at part of a schema.** Patterns take `*` and `?`, and `--exclude`
wins over `--include`:

```
pgsow --dsn "$DATABASE_URL" --apply \
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

A table with a rule pgsow cannot prove it satisfies is named, the rule is
quoted, and the table is left alone:

```
pgsow: 14 tables, 12 fillable, 2 refused (86% reach)

  refused:
    invoices     CHECK "invoices_total_positive" CHECK ((total > (0)::numeric))
                 — this cannot prove a generated row satisfies it, so it will
                 not write one
    shipments    foreign key cycle between shipments and orders — every key in
                 it is NOT NULL and not deferrable, so no order of single-row
                 inserts can satisfy it. Making shipments_order_fk deferrable,
                 or one of its columns nullable, would be enough.
```

That matters because the way a seed tool fails is not by crashing. It is by
inserting plausible rows that quietly break a rule nobody re-checked, so
everything downstream is tested against data the real system would reject.

So a CHECK constraint is never evaluated. There is no expression parser and no
guessing. There is a **closed set of exact shapes**, each with a satisfaction
you can point at — a length limit, a value set, exactly-one-of-these-columns —
and anything outside it refuses. Details and the full list:
[docs/](docs/).

## How well it works

Twenty schemas taken from real open-source projects — 2,354 tables — generated
for, applied to a real Postgres, and **not one row rejected**. The gate is
zero, not a percentage, because the database is the judge.

|  | by reasoning | with `--probe` |
|---|---:|---:|
| of every table in the corpus | 64.9% | **87.7%** |
| of the tables that can hold a row | 67.7% | **91.6%** |

Both are shown because they answer different questions. The second excludes
**100 tables that can hold no row from anybody**: 83 partitioned tables whose
dumps declare the parent and create the partitions at runtime, and 17 more
downstream of one.

<details>
<summary>Per schema</summary>

| schema | tables | by reasoning | with `--probe` |
|---|---:|---:|---:|
| PowerDNS | 7 | 100% | 100% |
| Hasura | 8 | 88% | 88% |
| Kong | 9 | 100% | 100% |
| Sourcegraph *(codeintel)* | 13 | 77% | 92% |
| listmonk | 16 | 100% | 100% |
| Ory Hydra | 18 | 83% | 94% |
| Harbor | 21 | 100% | 100% |
| Sourcegraph *(insights)* | 21 | 81% | 95% |
| Ory Kratos | 23 | 100% | 100% |
| Vaultwarden | 29 | 100% | 100% |
| hex.pm | 36 | 61% | 100% |
| Temporal | 37 | 100% | 100% |
| Plausible | 41 | 44% | 100% |
| Mattermost | 80 | 94% | 100% |
| Synapse | 134 | 99% | 99% |
| PostgREST *(test fixtures)* | 134 | 96% | 98% |
| Lago | 138 | 83% | 98% |
| Sourcegraph *(frontend)* | 180 | 43% | 83% |
| Discourse | 351 | 97% | 99% |
| GitLab | 1,057 | 40% | 77% |
| **total** | **2,354** | **64.9%** | **87.7%** |

Measured twice on separate runs, byte-identical both times, and identical on
Linux and Windows. Every schema is pinned to a commit, so the numbers are
reproducible rather than whatever a branch pointed at that day.

</details>

The corpus is **not written by this project**. A hand-written one only contains
the constructs its author remembered to handle, which measures agreement rather
than accuracy. Sources and licences are in `tests/corpus/sources.json`; fetch
them with `python tests/corpus/fetch.py`.

## Speed

Release build, which is what ships.

| | generate | SQL |
|---|---:|---:|
| 14 tables, 50 rows each | 28 ms | 63 KB |
| GitLab, 50 rows each | 0.3 s | 5 MB |
| GitLab, 1000 rows each | 4.9 s | 103 MB |
| Discourse, 100 rows into a live database | 3.4 s | 36,433 rows |

The SQL is streamed one statement at a time, so a large schema at a large row
count is a question of patience rather than of memory.

## What the data looks like

Column names are read the same way constraints are: a closed set of exact
shapes, matched on the whole name, its last two segments, or its last one.
`email` gets an address, `country_code` gets `SE`, `checksum` gets 64 hex
characters, `quantity` gets a number between 1 and 20.

- **Columns in one row describe the same person.** `first_name`, `last_name`
  and `email` are three readings of one number, so they agree by construction.
- **Rows in a child table describe the parent they point at.** Fill 3 users and
  7 notes and every note names the user it references.
- **Nothing generated can reach anybody.** Addresses use the RFC 2606 reserved
  domains, phone numbers the 555-01xx block, IPs the RFC 5737 ranges.
- **Unique columns stay unique.** Where being distinct and being coherent
  conflict, distinct wins — seven email addresses cannot come from three
  people.

Which names to cover was counted, not guessed: 5,509 text columns across the
corpus, of which 77% land on a known noun. The rest get an ordinary word and no
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
