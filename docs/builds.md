# The build shortlist

Six candidates survived the idea loop. This records all six, why each survived,
and what is holding the ones not being built. Build 1 is this repository.

| # | Build | Why it survived | Status |
|---|---|---|---|
| **1** | **Schema-aware seed data generator for Postgres** | Incumbent is a 788★ zombie; demand proven, nobody serving it | **This repo** |
| 2 | Offline-first event-day ops for kids' programs and community sport | No credible OSS; every incumbent is weakest at exactly the moment that matters | Blocked on one question |
| 3 | Registry-agnostic dependency provenance gate (anti-slopsquatting) | Threat confirmed and repeatable; every defence is vendor-shaped | Held |
| 4 | Query plan regression gate for CI | Nothing catches an ORM change turning an index lookup into a sequential scan *before* merge | **Second milestone of this repo** |
| 5 | Local subscription detection from email, no bank linking | Self-hosted trackers are all manual entry; the automatic ones demand Plaid | Held, with caution |
| 6 | Vulkan / WebGPU charting engine | Highest ceiling of the six | Held, with a correction |

## What is holding each one

**2 — event-day operations.** Rests entirely on one claim that has not been
verified: real, recurring access to actual events. If that is true it is the
strongest item on the list, because access is the one thing a competitor cannot
copy. If it is hypothetical it is the weakest, because it shares a shape
already rejected once — one-shot, high-stakes, non-technical operator, no retry
— and adds a legal surface, since guardian consent signatures carry weight.

**3 — dependency provenance.** Real and confirmed: when researchers re-ran
identical prompts ten times, 43% of hallucinated package names appeared on
every run, and `react-codeshift` — a conflation of two real tools — was
registered by someone in January 2026. The defences that exist are vendor-
shaped, checking against proprietary intelligence feeds. The open version would
score packages from public registry metadata alone. Held because it is
adjacent to build 1 in difficulty and further from anything we can dogfood.

**4 — query plan regressions.** Does not exist because its prerequisite is
expensive: plan regressions only appear at realistic data volume, so a
plan-diff gate needs a seeded database first. Build 1 produces exactly that.
This is the strongest structural insight in the list, and the reason 1 and 4
are one project with two milestones rather than two projects.

**5 — subscription detection from email.** The ghost of a previous project:
same domain, same merchant long tail, and the proposed "community-contributed
rules directory" is the same bet as that project's label list — which failed on
real receipts. It survives one genuine difference: email receipts are *text*,
and what killed the predecessor was the reading layer, not the parsing layer.
IMAP credentials remain a surface that project refused on purpose, and those
reasons still hold.

**6 — charting engine.** Conflates two engines. Vulkan is not available in
browsers; the web path is WebGPU, so "native and web" is two projects sharing a
name. And `lightweight-charts` is free, small and excellent, which makes it the
worst kind of incumbent to attack: there is no price to undercut and no
weakness to exploit.

## What was killed at the saturation step

Recorded because a rejected idea is only useful if the reason survives.

| killed | by |
|---|---|
| MCP gateways / security proxies | five-plus production open-source gateways already compared head to head |
| Kids' YouTube whitelisting | multiple commercial apps, YouTube's own approved-content mode, and two active OSS repos |
| Generic LLM observability | saturated |
| Splitwise clone | Spliit 2,852★ and split-pro 1,394★, both active |
| Linktree clone | LinkStack 3,749★ and littlelink 3,056★ |
| OpusClip clone | AI-Youtube-Shorts-Generator 4,606★ |
| Dynamic QR codes | field genuinely empty, but the people who feel the pain cannot self-host and the people who can already own a domain |
| Event photo sharing | WedUploader already sells the exact differentiator, and the market has a dozen vendors |

The pattern across those last five: **"open-source clone of a paid SaaS" is the
first idea everyone has**, so the good ones are taken, and the untaken ones are
untaken because the value was never in the code — it was in somebody running a
server forever.
