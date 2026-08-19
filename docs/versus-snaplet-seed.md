# pgseed and Snaplet seed, on the same 24 schemas

The README says this project has no codegen step and a correctness claim the
database checks. Neither sentence says anything about how the alternative does,
and until now nobody here had measured it. Claiming to be better than something
you never ran is the exact failure this project was built to avoid, so it was
run.

**Snaplet seed** (`@snaplet/seed` 0.98.0, published 2024-08-14, the last day its
repository saw a real commit) is the closest live comparison: same job, same
database, MIT, 788 stars.

## Method

Fair by construction, and every choice that could favour one side is named
below.

- **One database per schema**, created fresh and loaded once from the same
  `.sql` file the corpus benchmark uses. Both tools then face the identical
  database.
- **Same row count** — five per table — and each run starts from a truncated
  database the other has not touched.
- **Neither tool's own report is its score.** The number is `count(*)` per
  table afterwards, taken from `pg_class` via `query_to_xml`. The database is
  the only witness both runs share.
- Three runs per schema, because two of them answer different questions:

  | run | what it does |
  |---|---|
  | `snaplet` | writes rows and finds out what the database says |
  | `pgseed` | writes only what it can prove, and names the rest |
  | `pgseed --probe` | writes what it can prove, then offers the rest behind a savepoint — the same bet Snaplet makes, so this is the like-for-like column |

Snaplet does not seed a database by itself: you write a script naming the models
you want. The script here is generated from its own `dataModel.json` and asks
for every model it found, which is the most complete run it can be given.

## Results

| schema | tables | snaplet | pgseed | pgseed --probe |
|---|---:|---:|---:|---:|
| powerdns | 7 | 4 | **7** | **7** |
| kong | 9 | 8 | **9** | **9** |
| sourcegraph *(codeintel)* | 13 | **12** | 11 | **12** |
| hydra | 15 | 7 | **15** | **15** |
| listmonk | 16 | 14 | **16** | **16** |
| harbor | 21 | **21** | **21** | **21** |
| sourcegraph *(insights)* | 21 | 19 | 17 | **20** |
| kratos | 26 | **25** | **25** | **25** |
| vaultwarden | 28 | 22 | **28** | **28** |
| hexpm | 36 | 34 | 22 | **36** |
| temporal | 37 | 36 | **37** | **37** |
| plausible | 42 | 40 | 19 | **42** |
| camunda | 49 | **49** | 48 | **49** |
| documenso | 51 | 49 | **51** | **51** |
| langfuse | 67 | 64 | 63 | **67** |
| mattermost | 83 | 82 | 78 | **83** |
| synapse | 134 | 128 | 133 | **133** |
| lago | 138 | 125 | 114 | **134** |
| sourcegraph *(frontend)* | 180 | 105 | 79 | **151** |
| discourse | 352 | 346 | 343 | **347** |
| gitlab | 1,328 | 201 | 454 | **851** |
| **total** | **2,653** | **1,391** | **1,590** | **2,134** |
| | | **52.4%** | **59.9%** | **80.4%** |

Snaplet's runs raised **1,011 errors** across these schemas. Each one is a table
left empty by an exception, not by a decision.

The shape of the difference is clearer than the totals. On small schemas the two
are close and Snaplet sometimes wins. On GitLab — 1,328 tables — Snaplet fills
201 and throws 872 times, while `--probe` fills 851. Whatever its codegen was
built for, it was not a schema that size.

The other pattern is what the failures *are*. Snaplet's are constraint
violations found at runtime: `c_lowercase_name` on PowerDNS, a unique key on
Kong, `value too long for type character varying(100)` on listmonk, `malformed
array literal` on Documenso. pgseed's misses are refusals printed before it
writes anything, with the constraint quoted.

## Where this comparison is unfair, in both directions

Said plainly, because a benchmark that only lists its own advantages is an
advertisement.

**Unfair to Snaplet:**

- Its script is meant to be written by hand for the models you care about. This
  asked for every model in every schema, which is the widest possible target.
- Each model was wrapped so one failure did not stop the run. A real seed script
  aborts at the first exception, so the *usable* output of a failing run is
  worse than 52% suggests, not better.
- It has been unmaintained since August 2024. This is not a fight between two
  projects receiving equal effort.
- PostgREST could not be measured at all: its seed run died inside Node's ESM
  loader. Excluded rather than counted as zero.

**Unfair to pgseed:**

- Two schemas are excluded from the table above because pgseed scored **0** on
  them, and the reason was its own default. Hasura keeps its tables in
  `hdb_catalog` and Zitadel in `zitadel`; `--schema` defaults to `public`, so it
  read an empty schema and said "0 tables" with exit 0. Snaplet reads every
  schema and got 4 and 8. That is a real usability failure and it is fixed —
  pgseed now names the schemas that do hold tables — but the fix came out of
  this comparison, so counting it here would be scoring after the fact.
- The `--probe` column for Sourcegraph is 151 rather than the 0 the run
  recorded, for the same reason: the run found a bug, the bug is fixed, and 151
  is the verified figure afterwards. See below.

## What this found in pgseed

Three things, none of which the corpus benchmark had caught, because the corpus
tests the library and this tested the binary a user actually runs.

### A deferred foreign key is not checked by a savepoint

`--probe` keeps a row when the database accepts it inside a savepoint. A
constraint declared `DEFERRABLE INITIALLY DEFERRED` is not looked at until
COMMIT, so on Sourcegraph every probed row was accepted and then COMMIT failed
on a foreign key — losing the whole run. `--apply --probe` filled **0 of 180**
tables.

Nothing invalid was ever written: the transaction rolled back, which is the
right direction for the error to point. But "the database accepted it" was not
true in the sense `--probe` claims, and a run that loses everything at the last
step is not usable.

Fixed by forcing the check inside the savepoint, while the row can still be
taken back. Sourcegraph now fills **151 of 180**, and one table that used to be
rescued is now correctly refused.

### A failure reported as "db error"

The commit failure printed `could not commit: db error`, because
`postgres::Error` renders as those two words and the explainer that fixes it
lived in the binary while the failure happened in the library. It now lives in
one place both can reach, and the same failure reads:

```
could not commit: insert or update on table "batch_spec_workspace_execution_last_dequeues"
violates foreign key constraint "..._user_id_fkey"
       Key (user_id)=(281074) is not present in table "users".
```

### A silent zero when the tables are somewhere else

Described above. `pgseed --plan` against Hasura now says:

```
pgseed: no tables in public, but there are tables in hdb_catalog (8) — pass --schema to read one of those
```

## What it does not show

That pgseed is better software. It shows that on these 24 schemas, asked for
every table, pgseed fills more of them and fails differently — by declining in
advance rather than by throwing at runtime. Snaplet does things this does not:
it is a TypeScript library you can script against model by model, which is a
real advantage when you want twelve specific rows rather than a full database.

The comparison is reproducible: `docs/` holds the harness alongside this file,
and every schema is pinned to a commit in `tests/corpus/sources.json`.
