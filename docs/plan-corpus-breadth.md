# Corpus breadth: Prisma, Java, AI infrastructure — and four harness faults

> **Outcome, written after doing it.** The corpus went from twenty schemas to
> twenty-four, and from one dominant ecosystem to four. That was the plan. What
> the plan did not predict is that adding schemas would expose **four faults in
> the harness itself**, two of which had been silently mismeasuring published
> numbers since before the first release. Those are recorded first, because they
> matter more than the new schemas do.

## Why this was done

The corpus is the whole correctness argument, so its composition is part of the
claim. Counted, that composition was bad in one specific way: **ten of twenty
schemas were Go**, and there were none from Prisma, Django, SQLAlchemy,
Hibernate, Eloquent or Entity Framework, and no AI infrastructure at all.

That is not a breadth complaint. Every ORM emits characteristically different
DDL, so a corpus with one ecosystem in it measures agreement with that ecosystem
rather than accuracy. Django, for one, makes foreign keys `DEFERRABLE INITIALLY
DEFERRED` **by default** — the exact path where a stress test found a live bug
that nothing in the corpus exercised.

---

## The four harness faults

Each was found by a new schema, and each was already costing the old ones.

### 1. The GitHub contents API caps a directory listing at 1,000 entries

Silently. No error, no `truncated` flag, no pagination link the fetcher was
reading. Kratos's migrations folder holds **3,483** files and Hydra's **1,223**,
so both were fetched at exactly 1,000 and nobody was told.

Kratos was therefore replaying **99 of its 277** postgres migrations. Its
published score — 23 tables, 100% reach — was measured on a schema missing a
third of itself.

**Fixed** by listing through the git trees API with a `ref:path` tree-ish, which
scopes the recursion to the migrations folder rather than the repository: one
call instead of 433 for Langfuse, and it cannot truncate at this size.
Mattermost (213 files) and Vaultwarden (46) were under the old cap and re-fetch
byte-identical, which is the control saying the rewrite changed nothing else.

### 2. A dialect filter that silently excluded the shared migrations

Ory's fizz migrations use one file per dialect where the SQL differs, and one
shared file where it does not. Kratos was filtered on `.postgres.up.sql`, which
looks right and drops the **62 dialect-agnostic** migrations — one of which adds
a column a later constraint names. Hydra beside it was already filtered
correctly, on `.up.sql` with the other dialects named.

Complete, Kratos is **31 tables at 94%**, not 23 at 100%. The number went down,
which is what an honest correction usually looks like.

### 3. A file boundary is a statement boundary, and the fetcher did not say so

Every migration runner executes each file separately, so the last statement in a
file needs no terminator — and Prisma routinely leaves it off. Concatenated,
that statement swallows the first statement of the next file.

Langfuse merged an `INSERT INTO models` with the `ALTER TABLE ... ADD COLUMN
"project_id"` behind it. The merged statement read as an `INSERT`, was filtered
out with the seed data, and **seven constraints downstream of that column** were
counted as lost.

Counted after fixing it: **136 missing terminators across six of the eight
directory schemas** — 8 in Mattermost, 23 in Kratos, 50 in Hydra, 52 in Langfuse
— every one of which had been merging statements since the schema was added.

### 4. A statement does not always begin where it starts

`ADD COLUMN "organisationId" TEXT; -- [CUSTOM_CHANGE] ...` ends one statement at
the semicolon and leaves the trailing comment at the front of the next, so the
next statement's *head* reads as a comment and the filter drops it. Documenso
writes exactly that, and the `ALTER TABLE "Team"` behind it never ran.

**Fixed** by reading a statement's head past any comment in front of it, in one
place both readers share.

### And one that was not a fault, but was measuring the wrong thing

`max_lost_constraints` counted every failed constraint statement as a loss,
including ones that failed with **already exists** — where the constraint is
present, put there by an earlier migration whose `DROP` the filter removed — and
ones whose table failed to create, which never enter the denominator at all. The
second of those had been described correctly in a comment and never implemented.

With the counter measuring what it claims, **seven of the eight non-zero
ceilings went to zero** and stayed there. GitLab's two are real: foreign keys the
replay cannot build against a partitioned parent.

---

## The schemas

| schema | tables | ecosystem | why this one |
|---|---:|---|---|
| **Langfuse** | 81 | Prisma / TypeScript | The first AI-infrastructure schema here. 432 migrations, each in its own folder, and 576 `@map` directives — so its columns end snake_case after a long chain of renames, which is a harder replay than any snapshot. |
| **Documenso** | 61 | Prisma / TypeScript | Prisma with **no** `@map` at all, so its columns really are `"emailVerified"` and `"avatarImageId"` — quoted camelCase, which nothing else in the corpus had. |
| **Camunda 7** | 49 | Java / MyBatis | The corporate shape: unquoted uppercase identifiers, `varchar` primary keys, no sequences. Six dialects share one folder, which is why the filter here names what to keep rather than what to drop. |
| **Zitadel v3** | 30 | Go | A pre-release schema, pinned as one. Dense `CHECK (col <> '')`, a schema of its own, and the multi-tenant composite-key shape that found the emitter bug below. |

Zitadel's older `cmd/setup` folder looks like a migration set and is not one:
sampled, its files carry **no** `CREATE TABLE` at all — they are ALTER and INDEX
patches against tables Go creates. Named here so nobody re-proposes it.

---

## What the new schemas found in the tool

### Foreign keys that share a column wrote rows no parent held

Langfuse's `in_app_agent_runs` carries `(conversation_id, project_id)` and a
second key that also writes `project_id`. The emitter drew one whole parent row
per key into a map keyed by column, so the second key overwrote the shared
column and the first was left **half from one parent row and half from another**
— a pair neither parent ever held. Postgres rejected it, which is how it was
found, and a row the database refuses is a row this should have refused first.

Measured before deciding what to do: **27 of 2,586 tables** have foreign keys
that share a column. One percent — but the failure mode is a wrong row rather
than a missing one, so the count was never the argument.

Both halves were built:

- **Refuse** where the two keys can genuinely disagree — `(instance_id,
  granting_organization_id)` and `(instance_id, granted_organization_id)` both
  pointing at organizations, where two different organization rows need not
  share an instance.
- **Satisfy** where it is provable. Zitadel's `projects` carries
  `(instance_id) -> instances(id)` **and** `(instance_id, org_id) ->
  organizations(instance_id, id)`. Drawing both from the organization row is
  correct, and the reason is checkable statically: `organizations.instance_id`
  is itself a foreign key to `instances.id`, so the value came out of the
  instances pool. The emitter now takes the widest key first and lets a key
  whose columns are already supplied stand aside.

This is the multi-tenant shape, and it is most of what real SaaS schemas look
like. It was worth 5 points on Hydra, 2 on Langfuse and 4 on Zitadel.

### A camel hump is a word boundary

`nouns::of` lowercased a column name and split it on underscores, so Documenso's
`twoFactorSecret` became `twofactorsecret` and matched nothing. Splitting the
hump first gives `two_factor_secret`, which the closed set already understood —
a normaliser in front of the matcher rather than a second matcher beside it. A
run of capitals stays one word, and a name that was already snake_case comes back
unchanged, which is the property the other twenty-three schemas depend on.

Worth **101 columns**, and it takes Documenso to 90% named, Langfuse to 89%,
Camunda to 90% and Zitadel to 97%.

### A unique address repeated, and only above 254 rows

Re-running the volume benchmark after the corpus grew turned up a duplicate key
on Discourse's `screened_ip_addresses.ip_address` at a thousand rows, and none
at fifty. The generator counted the low octet modulo 254 and the octet above it
modulo 256, so the digits did not carry together and step 0 and step 254 were
both `10.0.0.1`. One radix per octet fixes it, and an `inet` is an address
rather than a network, so the full byte was available the whole time.

Reading the arm beside it found the same fault twice more: `cidr` and `macaddr`
ignored uniqueness entirely and drew from the random stream. No corpus schema
has a unique column of either type, so nothing would have caught those until
somebody's schema did. Both now count.

The unit test asserting this walks two thousand rows, because the fault is
invisible below the first carry — which is exactly why a gate at fifty rows had
never seen it.

### And two surveys that had quietly stopped surveying

The CHECK survey iterated a hard-coded list of eighteen schema names that had
drifted from the manifest — so Hydra, listmonk and everything added since were
never in it, and their CHECK constraints had never been looked at. It also
judged with `interpret` where `classify` uses `interpret_all`, so it overstated
what was not understood. Both now read the manifest and judge the same way.

The column survey lowercased every name before counting, which would have hidden
the camelCase problem it exists to find.

---

## Results

Zero rejections across all twenty-four schemas. The gate is zero, not a
percentage, because the database is the judge.

|  | before (20 schemas) | after (24 schemas) |
|---|---:|---:|
| of every table, by reasoning | 64.9% | **66.6%** |
| of every table, with `--probe` | 87.7% | **88.1%** |
| of tables that can hold a row, by reasoning | 67.7% | **69.3%** |
| of tables that can hold a row, with `--probe` | 91.6% | **91.6%** |
| tables | 2,354 | **2,586** |
| text columns landing on a noun | 77% | **79%** |

New schemas, by reasoning and with `--probe`: Documenso **100% / 100%**,
Camunda **98% / 100%**, Langfuse **91% / 100%**, Zitadel **17% / 37%**.

Zitadel is the weak one and the reason is legible: its v3 schema maintains a
`login_names` projection with INSERT triggers that write into other tables, and a
trigger that writes rows this tool did not plan is a refusal. That is the schema
being unusual rather than the tool being wrong, and it is quoted rather than
hidden.

---

## The rule, restated

`sources.json` said **fetched, not generated**. That is the wrong statement of a
right principle, and the corpus already broke the literal reading of it: one
schema arrives as SQL embedded in a **Lua** file, eight are concatenated out of
migration directories by a script in this repository, and the loader filters
statements and supplies a function by hand. Transport is already a
transformation this project performs.

What the doctrine forbids is **authorship**. A schema written here would only
contain the constructs its author remembered to handle, which measures agreement
rather than accuracy. So the rule now reads: **authored elsewhere, and every loss
measured.**

Under that rule, generating DDL by running a project's own migration tool is
permitted. It is still not done, for two practical reasons and one fact:

1. **The pin weakens.** Today reproducibility is a commit SHA. `manage.py
   sqlmigrate` needs a Python version, a Django version and a full dependency
   resolution — reproducible in principle rather than by anybody.
2. **It runs the target project's code.** `sqlmigrate` imports `settings.py` and
   the models. That is arbitrary third-party execution in a scheduled CI job.
3. **There is nothing to fetch anyway.** Checked: `saleor`, `netbox`, `sentry`
   and `superset` contain **no `.sql` files** between them across their entire
   repositories. The Django and SQLAlchemy gap is a supply problem, not a policy
   one, and closing it means generation or nothing.

If it is ever done, the conditions are already decided: a single documented
command against a pinned commit, run in a sandbox rather than in the corpus job.

## Zitadel's 17%, measured rather than assumed

The obvious reading is that the INSERT triggers are the problem. They are not.
Of Zitadel's 47 root refusals, **46 are on one table** — `users` — and 43 of
those are a single CHECK shape:

```sql
CHECK (((type = 'machine') AND (email IS NULL)) OR (type = 'human'))
CHECK (((type = 'machine') AND (last_name IS NULL)) OR ((type = 'human') AND (last_name <> '')))
```

A tagged union: one discriminant column decides which of the others must be
NULL and which must be filled. Satisfying it means picking a value for `type`
first and then reading every other CHECK in that light, which is a different
shape of reasoning from anything in the closed set today.

The trigger refusals are 12 of the 47, and refusing them is **correct rather
than cautious**. `login_names` is a projection: `apply_user_insert_to_login_names`
fires `AFTER INSERT ON users` and derives its rows from the user, the
organisation's verified domains and the instance's domain setting. A row
written into it directly would describe a login name derived from nothing,
which is worse than no row. The right behaviour is to fill `users` and let the
trigger fill `login_names`, which is exactly what happens the moment `users`
becomes fillable.

Measured, with `what_the_database_fills_for_you`: after a probed run, **0 of
Zitadel's 17 still-refused tables hold any rows**. The triggers never fire
because their source table is refused. So the whole schema turns on `users`,
and `users` turns on the tagged union.

Worth building? The count says be careful. The shape is **43 constraints in
Zitadel, 4 in Sourcegraph and 1 in GitLab** — one schema, essentially. Unlocking
`users` would gain it and about five pure dependents, roughly six tables of
thirty, taking Zitadel's reasoning number to about where `--probe` already
puts it and moving corpus reach by about 0.2 points.

That is the same size as the two levers already abandoned here on evidence. It
is left undone for now, with one caveat recorded honestly: this is the one
place where corpus frequency and real-world frequency plausibly disagree —
single-table inheritance is common in Rails and in hand-written Postgres, and
twenty-four schemas containing one instance of it is weak evidence either way.

---

## Left undone, deliberately

- **Django, SQLAlchemy, TypeORM, Eloquent, Entity Framework.** No fetchable SQL
  exists. Deferred behind the restated rule above rather than quietly dropped.
- **Partial overlap between foreign keys.** Two keys sharing a column where
  neither subsumes the other are refused. Choosing both parents from the same
  tenant would satisfy them, and it needs the pool searched rather than indexed.
  Not built; the provable half was, and it covered most of the shape.
