# Plan: asking the database, and being wrong about the ceiling

Written after the question *"is there no actual viable way to sustain 80% or
more across the 20 schemas?"*, to which the recorded answer was no. The
recorded answer was wrong. This is how, and what was done about it.

---

## The wrong conclusion, and why it was wrong

Reach reached 63% and stopped. Every route to raising it had the same shape:
read more of the schema, widen a closed set, refuse less. The refusals left
over were counted and attributed:

```text
  592  a CHECK outside the closed set (which includes triggers and partitions)
  351  contagion, from a parent that was itself refused
   55  unique keys that cannot both be enumerated
    4  a column type with no generator
    4  a parent in a schema that was never read
```

GitLab is 464 of the CHECK number, and 261 of its triggers are
`SELECT ... INTO NEW.col FROM parent WHERE ...` — a sharding key backfilled from
a lookup that can return NULL, on a column a CHECK then insists on. The
conclusion drawn was that those refusals are *correct*, and that reaching 80%
would mean writing rows the tool cannot show are right.

Every step of that is true. The conclusion still does not follow, because it
answers a question the doctrine does not ask.

> never emit a row that cannot be **shown** to satisfy every constraint that
> was read

"Shown" is not "reasoned about". **A row Postgres has accepted has been shown
to satisfy every constraint** — including the ones nobody read, which is more
than static analysis can offer, not less. The 261 sharding triggers *can* yield
NULL; whether they *do*, for the rows actually generated, is a question with an
answer, and the answer is in the database rather than in the trigger body.

A savepoint is what makes that question askable and retractable.

## The measurement, before the feature

The rule here is measure first, so a survey forced every table into the
fillable set, ran each INSERT behind its own savepoint, and counted what the
database kept.

```text
  63.2%   reasoning alone
  86.2%   with the database asked
```

Per schema, the two that were dragging the average:

| | reasoning | probing |
|---|---:|---:|
| GitLab | 40% | **77%** |
| Sourcegraph frontend | 42% | **83%** |
| hex.pm | 61% | **100%** |
| Discourse | 92% | 98% |

And the schemas where reasoning was already right — Hasura at 75%, Kong at 89%,
Plausible at 45% — gain nothing, which is the good sign. Those refusals were
real, and probing agrees with them.

## What was built

`src/probe.rs`, `--probe`, and one change to the emitter.

**One pass, not two.** Every table goes in dependency order with nothing
refused, and each statement is executed inside `SAVEPOINT pgsow_probe`. Kept,
it is released; refused, it is rolled back and the table stays refused. Two
passes would have been tidier and would not work: a rescued parent has to be
written before the child that points at it, and for a parent whose key has no
default there is nowhere to find that key but the emitter's own pool.

**Everything is savepointed, including the statements that belong to no table.**
`SET CONSTRAINTS ALL DEFERRED` fails on a schema with nothing deferrable, and
an unprotected failure poisons the whole transaction. In the first survey that
showed up as three schemas filling nothing at all.

**The guarantee for understood tables does not move.** A table `classify`
accepted that fails under a probe is an error that aborts the run. It is not a
smaller number, because the whole point of separating the two is that one of
them is a promise.

### The bug that guarantee caught

It caught one immediately, on four schemas:

```text
sourcegraph_insights: BROKEN PROMISE — dashboard was said to be fillable
                      and is not: 23503 violates dashboard_tenant_id_fkey
```

The key pool records what a table's INSERT *would* write, and children draw
their foreign keys from it. When a probe rejected a parent, the pool went on
holding rows that did not exist, and the next child pointed at them. The
emitter now asks what became of each statement — `Kept`, `Rejected`, `Stop` —
and only pools what was kept.

Without the promise being enforced, that would have shown up as four schemas
quietly reaching a lower number, and it would have been believed.

## What it costs

Stated rather than implied, because an opt-in feature that writes to a database
deserves it:

- **It writes.** A probe is a real INSERT behind a savepoint. `--probe` is
  guarded exactly as `--apply` is: the host must be this machine, or `--remote`.
- **Triggers fire.** Anything they do inside the transaction rolls back with
  it. A foreign data wrapper or an untrusted extension reaching outside one
  does not.
- **Sequences do not roll back.** A refused probe spends the numbers it drew.
- **It is not a solver.** One row, offered once, kept or discarded. No
  narrowing, no retry. Narrowing a value until it fits would be the expression
  evaluator this project has refused to build, wearing a disguise.

## What is left

86.2% is measured, not a ceiling. The 14% still refused after asking is now the
interesting set, because every one of those refusals has been confirmed by the
database rather than merely reasoned to. Plausible at 48% is the clearest case:
23 tables refused, 1 rescued, so that schema really is out of reach and the
reason is worth reading rather than guessing at.
