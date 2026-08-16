# pgsow

**Point it at a Postgres connection string. It reads the schema and produces
data that satisfies it — or names the table it will not touch, and why.**

```
pgsow --dsn postgres://localhost/mydb
```

No codegen step. No generated client. No API key. No runtime — one binary.

## Status: first milestone

It **introspects and classifies**. It reads the schema, works out the insert
order, and decides for every table whether it could be filled or must be
refused. **It does not generate values yet**, and that is deliberate.

The number that decides whether the rest is worth writing is *what fraction of
real tables can be ordered and classified without ambiguity*. That is
measurable before a single value is invented, so it is being measured first.

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

So a CHECK constraint is read, quoted, and **never solved**. A partial solver
that silently handles `total > 0` but mishandles `total > 0 OR status =
'void'` is worse than no solver at all, because the cases it gets wrong look
exactly like the cases it gets right.

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
| Refusal contagion | a table whose required key points at a refused table is refused too, all the way down |

## Why this exists

The category has proven demand and no maintained free option. `neosync` is
archived. `supabase-community/seed` has 788 stars, 25 open issues including
unpatched high-severity advisories, and its last real commit was August 2024 —
it also requires a codegen step and a generated TypeScript client.

Determinism is *not* the differentiator; that tool has it too. The
differentiators are: no codegen, no client, no runtime, no API key, and being
alive.

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
