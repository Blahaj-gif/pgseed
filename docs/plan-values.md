# Plan: what the values actually say

Written after the question *"why do we have alpha, bravo — headers and not
actual real nouns and items people use?"*, which is a fair question with a bad
answer behind it.

---

## The answer that was there, and why it was only half right

Every text column was filled from the NATO alphabet, and the comment above the
list said so on purpose:

> Deliberately dull: this generates *valid* data, not realistic data, and
> pretending otherwise invites somebody to demo with it.

That reasoning is sound about **correctness** and useless about **use**. It
protects against one failure — somebody mistaking generated rows for real ones
— by guaranteeing another: nobody wants the output. The incumbent this project
is measured against ships an entire library for this, and it is the single most
visible difference between the two.

Both things can be true at once, and the resolution is not to abandon the
discipline but to apply it here as well.

## The discipline, applied to names

`checks` reads a CHECK constraint by matching a **closed set of exact shapes**
and refusing everything else. `indexes` does the same for index expressions,
`triggers` for trigger bodies, `partitions` for bounds. A column name is the
same kind of problem: it is a string that either says something exact or does
not.

So: match the whole name, then its last two segments, then its last one, each
against an exact list. Never a substring — `description_id` is not a
description, and a fuzzy match there fills a foreign key with a sentence.

### Where the list comes from

Not from memory. `tests/columns.rs` loads all twenty corpus schemas into a real
Postgres and counts every text-typed column: **5,509 of them**. The head of the
ranking is the list:

```text
  666 name    463 id      290 type    230 key     217 path
  172 url     163 desc.   134 file     88 etag     79 token
   78 email    75 value    73 message  70 version  69 code
```

`name` on its own is ambiguous and its qualifier settles it: `first_name`,
`file_name` and `queue_name` are three different things, and 666 columns end
that way.

A handful of nouns — city, street, postal code, company — are barely in that
ranking, because twenty open-source backend schemas are infrastructure and not
a CRM. They are covered anyway. **That is the one thing in this project
included on judgement rather than measurement, and it is said out loud rather
than buried.**

### What it reaches

The same test reports coverage, so this is measured rather than claimed:
**4,216 of the 5,509 text columns land on a noun — 77%.** The remaining 1,293
get an ordinary word and no claim at all.

The biggest gaps left are `value` (60 columns) and `code` (38). Both are
genuinely ambiguous — a `value` column holds whatever its `key` column says it
holds — and guessing at them is exactly the thing this refuses to do elsewhere.

## What must not break

Three properties the rest of the project already leans on, each now a test.

**ASCII only.** `checks` reads `octet_length(col) <= N` and
`char_length(col) <= N` as the same ceiling, which is true exactly while every
generated string is ASCII. One accented surname makes that silently false and
starts producing rows Postgres rejects on a constraint the tool believed it had
satisfied. The first attempt at the word lists contained `Zieliński`, so this
is not hypothetical.

**Distinct on demand.** A column under a unique key must differ every row —
provably, not probably. The lists are read as an **odometer**: digits of the
step, with a counter appended once the lists are exhausted, so `(digits, carry)`
reconstructs the step. Two bugs came out of this and both are now tests:

- a username built from an *initial* plus a surname threw away exactly the
  information that made two steps different, and repeated on row two;
- a hex identifier with a variable-width tag collided at row 176, because
  `<25 random chars>b` and `<24 random chars>b0` are the same length.

**It fits, or it declines.** A value too long for its column is refused, not
truncated, and the plain generator — which is built for tight columns — takes
over. `ada.love` is not an email address and is not distinct from the next one
either.

## Coherence, and its limit

`first_name`, `last_name`, `display_name`, `username` and `email` in one row are
**five readings of one number**, so they agree by construction rather than by
bookkeeping. A row that says *Ada* and `amara.adeyemi@` is worse than one that
says `bravo` twice, because it looks right and is not.

Where agreement and distinctness conflict — a surname under a unique key —
**distinctness wins and the agreement is what is given up**. That ordering is
the doctrine, not a preference.

Nothing else coheres. A `city` and a `country` in one row are drawn
independently and may not belong together. Stated, not fixed.

## Nothing generated can reach anybody

Generated data ends up in staging systems, and staging systems send mail and
make requests. So: addresses on the RFC 2606 reserved domains, telephone
numbers in the 555-01xx block reserved for fiction, IP addresses in the RFC 5737
documentation ranges. Where a reserved range runs out — there are exactly one
hundred fictional telephone numbers — this **declines** rather than inventing a
number that rings somebody.

## What it cost, and what it bought

One shape was widened along the way, because the first application-shaped schema
this was pointed at hit it immediately: `char_length(title) > 0`. A floor on the
length is satisfied by padding, and refused where the column has no room to pad
into — a `varchar(8)` obliged to hold twelve characters has no satisfying row,
and writing eight anyway is the silent pass this project exists not to do.

Eight of those exist across the whole corpus. It is worth reading anyway,
because a schema somebody wrote by hand rather than generated is full of them,
and that is the reader being served.
