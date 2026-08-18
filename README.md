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
| `--probe` | offer the refused tables to the database and keep the rows it accepts |
| `--remote` | write to a database that is not on this machine |

The last two are the only guards, and both are questions rather than guesses.
Reading a database name for the word `prod` stops nobody who called theirs
`main` and annoys everybody whose local copy is called `myapp_production_dump`.
So instead: **is the host this machine**, and **do the target tables already
hold rows** — two facts a person can answer instantly and a tool cannot.

## Reach, on twenty schemas nobody here wrote

| schema | tables | fillable | reach |
|---|---:|---:|---:|
| PowerDNS | 7 | 7 | 100% |
| Hasura | 8 | 6 | 75% |
| Kong | 9 | 8 | 89% |
| Sourcegraph *(codeintel)* | 13 | 10 | 77% |
| Harbor | 21 | 21 | 100% |
| listmonk | 16 | 16 | 100% |
| Ory Kratos *(replayed migrations)* | 23 | 23 | 100% |
| Ory Hydra *(replayed migrations)* | 18 | 15 | 83% |
| Sourcegraph *(insights)* | 21 | 17 | 81% |
| hex.pm | 36 | 22 | 61% |
| Vaultwarden *(replayed migrations)* | 29 | 29 | 100% |
| Temporal | 37 | 36 | 97% |
| Plausible | 41 | 18 | 44% |
| Mattermost *(replayed migrations)* | 80 | 75 | 94% |
| PostgREST *(test fixtures — deliberately awkward)* | 134 | 129 | 96% |
| Synapse | 134 | 126 | 94% |
| Lago | 138 | 115 | 83% |
| Sourcegraph *(frontend)* | 180 | 75 | 42% |
| Discourse | 351 | 324 | 92% |
| GitLab | 1,057 | 420 | 40% |
| **total** | **2,353** | **1,492** | **63%** |

Fetched with `python tests/corpus/fetch.py`; sources and licences in
`tests/corpus/sources.json`. Three are *replayed* migration directories rather
than a snapshot of a finished schema, which only reaches the real shape if
every migration in it applies — so the harness counts the constraints a failed
one costs, and each schema has a ceiling on that which a regression trips. The corpus is deliberately not written here — a
hand-made one would only contain the constructs its author remembered to
handle, which measures agreement rather than accuracy.

## Asking the database — `--probe`

Reach by reasoning alone sits at 63%, and for a long time the conclusion here
was that 80% was structurally unreachable: the remaining tables are refused for
a CHECK outside the closed set, a trigger whose body might raise, a partition
bounded by a range, and reading more of them has a floor.

**That conclusion was wrong, and it was wrong in an interesting way.** It
answered *can this be reasoned to be correct?* when the rule asks something
weaker and better: never emit a row that cannot be **shown** to satisfy every
constraint that was read. A row Postgres has accepted has been shown to satisfy
every constraint, including the ones that were never read at all — and a
savepoint makes that a question you can ask and take back.

| | reach | GitLab | Sourcegraph |
|---|---:|---:|---:|
| reasoning alone | 63.2% | 40% | 42% |
| with `--probe` | **86.2%** | **77%** | **83%** |

Each table's INSERT goes in behind a savepoint. Kept, it stands; refused, it
rolls back and the refusal is reported exactly as before. Without `--apply` the
whole transaction is rolled back at the end and only the accepted SQL is
printed, so a database you can write to gives you a seed file for one you
cannot.

**The guarantee for the understood tables does not move.** One of them failing
under a probe is an error that aborts the run, not a quietly smaller number —
which is how the key-pool bug below was caught rather than absorbed.

What it costs, stated rather than implied:

- **It writes.** A probe is a real INSERT. The same guard that protects
  `--apply` protects this, and `--probe` alone still rolls everything back.
- **Triggers fire.** Whatever a trigger does inside the transaction is rolled
  back with it; whatever it does outside one — a foreign data wrapper, an
  untrusted extension touching the filesystem — is not.
- **Sequences do not roll back.** A refused probe still spends the numbers it
  drew.
- **It is not a solver and does not retry.** A row is generated exactly as it
  would have been, offered once, and kept or discarded. Narrowing a value until
  it fits would be the expression evaluator this project does not have, wearing
  a disguise.

The report keeps the two apart, because a table filled because it was
understood and a table filled because the database allowed it are different
kinds of confidence:

```
pgsow: 1057 tables, 418 fillable, 639 refused (40% reach)
  of the refused, the database accepted 392 and refused 247 (77% reach with it asked)
```

## Status

It introspects, classifies, and **generates**. Point it at a database and it
writes SQL; `--plan` reports what it would do and writes nothing.

Every row it produces is checked by the only authority that matters: a test
generates for each corpus schema at the default fifty rows, applies the result
to a real Postgres, and fails if a single statement is rejected. It currently
generates **1,906 statements across the twenty schemas and Postgres accepts
every one** — with every one of their 456 triggers installed, and their 106
partitioned tables read. The gate is zero, not a percentage — the whole thesis is that the
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
| `char_length(col) >= N`, `> N` | padded up to, where the column has room, and refused where it has not |
| `col IS NOT NULL` | a column this never writes NULL into |
| `octet_length(col) = N` / `<= N` | a fixed width, or a ceiling |
| `col > N`, `col >= N` | a floor on the generated number |
| `col = lower(col)` | every string generated is lowercase |
| `col IS NULL OR …` | writing NULL satisfies the whole disjunction |
| `num_nonnulls(a, b[, c]) = 1` | fill one, NULL the rest |
| `jsonb_typeof(col) = 'object'` | emit a value of that type |
| `cardinality(col) <= N` | every array written holds one element |
| `col = ANY (ARRAY[…])` | write one of the listed values |

Exclusion constraints are read as the checks they are and refuse. Not seeing a
constraint is not the same as satisfying it.

Three outcomes, never two: **filled · refused · could-not-read.**

Exit codes follow: `0` everything fillable, `1` something refused, `2` the
schema could not be read — so it composes in a script without anybody parsing
prose.

## The data itself

Every text column used to be filled from the NATO alphabet — `alpha`, `bravo`,
`charlie` — on the argument that a tool producing *valid* data should not dress
up as one producing realistic data. That argument is right about correctness
and useless in front of a screen. So column names are now read the same way
CHECK constraints are: **a closed set of exact shapes**, matched on the whole
name, its last two segments, or its last one. Nothing is guessed from a
substring, because `description_id` is not a description.

```sql
INSERT INTO "public"."users" ("email", "username", "first_name", "last_name", "display_name", "timezone", "last_login_ip", "api_token", "state") VALUES
  ('ada.achebe@example.com',   'ada.achebe',   'Ada',   'Achebe',  'Ada Achebe',   'Europe/Berlin',  '192.0.2.160',   '333122c6…', 'suspended'),
  ('amara.adeyemi@example.org','amara.adeyemi','Amara', 'Adeyemi', 'Amara Adeyemi','America/Chicago','192.0.2.21',    'ce8327aa…', 'suspended');
```

Which names to cover was **counted, not remembered**: `tests/columns.rs` reads
every text column in the twenty corpus schemas — 5,509 of them — and ranks the
names. `name` appears 666 times, `id` 463, `type` 290, `key` 230, `path` 217,
`url` 172, `description` 163, `email` 78. That ranking is the list, and the same
test reports what the list still misses: **4,216 of the 5,509 land on a noun,
77%**, and the largest gaps left are `value` at 60 columns and `code` at 38 —
both genuinely ambiguous, which is why they are still gaps.

Three properties hold, and each is a test rather than an intention:

- **Columns in one row describe the same person.** `first_name`, `last_name`,
  `email` and `display_name` are four readings of one number, so they agree by
  construction rather than by bookkeeping. A row saying *Ada* and
  `amara.adeyemi@` is worse than one saying `bravo` twice, because it looks
  right and is not.
- **A unique column is still provably distinct.** The word lists are read as an
  odometer and a counter is appended once they are exhausted, so two rows
  cannot collide — distinctness that can be shown rather than distinctness that
  is very likely. Where distinctness and agreement conflict, distinctness wins
  and the agreement is the thing given up.
- **Nothing generated can reach anybody.** Addresses are on the RFC 2606
  reserved domains, telephone numbers in the 555-01xx block reserved for
  fiction, and IP addresses in the RFC 5737 documentation ranges. Generated
  data ends up in staging systems, and staging systems send mail.

A column whose name says nothing exact — `value`, `data`, `payload` — still
gets an ordinary word rather than a claim. And a value that will not fit the
column is **declined rather than truncated**: `varchar(8)` gets the old
tight-fitting generator, not `ada.love`.

What this does *not* do is make a row coherent beyond that: a `city` and a
`country` in one row are drawn independently and may not belong together. The
person columns are the exception, and they are the exception on purpose.

## What it handles today

| | |
|---|---|
| Insert order | topological, ties broken by name so runs are reproducible |
| FK cycles | broken through a nullable key, or by deferring a deferrable one |
| Unbreakable cycles | refused, naming the constraint that would have to change |
| Composite keys | referencing and referenced columns kept in declared order |
| Column names | 40-odd shapes — email, person, path, URL, host, MIME type, digest, slug, currency, locale, timezone — matched exactly, never by substring |
| Types | integers, numerics with precision, text with declared length, uuid, dates, timestamps, json, bytea, enums with their labels, domains, arrays |
| Generated & identity columns | left out of the insert entirely, not defaulted |
| Refusal contagion | a table whose required key points at a refused, or unread, table is refused too, all the way down |
| Probing | each refused table offered to the database behind a savepoint, and kept if it is accepted |
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
