# Proposed optimizations — proven, NOT applied

Everything in this document has been **measured against a copy of the real
1.33M-row production database** and is presented for a decision. The proposed
expression indexes have **not** been applied to production or the live database.
The independent multi-key ordering correctness fix described below is present in
the source tree, with a regression test, because it prevents a requested second
sort key from being silently ignored even without these indexes.

Method is the one that survived scrutiny elsewhere in this audit: indexes are
toggled *in place* so one server process, one binary and one page cache serve
both arms; arms are interleaved round by round; results are scored with
`tools/ab/stats.py` (Mann-Whitney U + bootstrap 95% CI).

## Coverage: what had and had not been measured

Before this pass, roughly 20 endpoints had been benchmarked. A sweep of **56
user-facing GET endpoints** was then run against the production copy. It found
six previously-unmeasured endpoints in the 200–390 ms range:

| Endpoint | Median | Previously measured? |
|---|---|---|
| `/items?sortBy=Random` | 388.5 ms | no |
| `/items?sortBy=CommunityRating` | 382.8 ms | no |
| `/items?sortBy=PremiereDate` | 367.6 ms | no |
| `/items/languages` | 343.6 ms | no |
| `/items/{id}/similar` | 292.9 ms | no |
| `/items/certifications` | 288.4 ms | no |

Endpoints confirmed already fast (no action needed): `/items/{id}` ~2 ms,
`/shows/{id}/episodes` (12,138-episode series, limit 100) ~14 ms,
`/items/{id}/instantmix` ~8 ms, `/library/*`, `/users/me`, `/items/filters`.

## Proposal 1 — four missing sort expression indexes

**Status: proven at SQL level; end-to-end confirmation in progress.**

### The defect

`get_by_filter` renders several sorts as **COALESCE expressions**:

| Sort | ORDER BY expression |
|---|---|
| `PremiereDate` / `ProductionYear` | `COALESCE(released_at, digital_released_at)` |
| `CommunityRating` | `COALESCE(rating_audience, rating_critic)` |
| `DigitalReleaseDate` | `COALESCE(digital_released_at, released_at)` |
| `Runtime` | `COALESCE(runtime, 0)` |

There is no index on any of these expressions. `idx_media_released_at` exists but
a plain column index cannot serve `COALESCE(...)`. So every one of these sorts
does a full scan plus `USE TEMP B-TREE FOR ORDER BY` over 1.33M rows. This is
**the identical defect already fixed for `DateCreated`**, simply on four more
sorts that were missed.

### Measured effect (SQL level, unfiltered shape — what `/items?sortBy=X` emits)

| Sort | Before | After | Gain | Temp b-tree | Rows identical |
|---|---|---|---|---|---|
| `Runtime` | 2214.7 ms | **0.4 ms** | **6266×** | eliminated | ✅ |
| `CommunityRating` | 1128.0 ms | **0.4 ms** | **3055×** | eliminated | ✅ |
| `DigitalReleaseDate` | 1510.2 ms | **6.4 ms** | **235×** | eliminated | ✅ |
| `PremiereDate` | 898.1 ms | **8.8 ms** | **103×** | eliminated | ✅ |
| `SortName` (control) | 0.8 ms | 0.5 ms | 1.8× | already indexed | ✅ |

Plan changes from `SEARCH … + USE TEMP B-TREE FOR ORDER BY` to
`SCAN media USING INDEX …`, exactly as it did for `DateCreated`.

### A false start worth recording

The first attempt measured these with `kind IN ('movie','series')` in the WHERE
clause and concluded the indexes were useless (0.9×–2.1×, plan unchanged). That
was wrong: with a type filter SQLite uses the kind index to filter and must sort
the subset regardless. The endpoint issues the sort with **no type filter**, and
that is the shape where the index applies. This is the same trap that produced
the reverted `EXISTS`→`IN` change — *benchmark the shape the server actually
emits.*

### Measured effect (END-TO-END, the real endpoint)

SQL-level gains do not always survive, so this was re-measured at the HTTP layer
with indexes toggled in place, interleaved, n=25–30 per arm:

| Endpoint | Before | After | Gain | 95% CI | p |
|---|---|---|---|---|---|
| `/items?limit=50&sortBy=DigitalReleaseDate` | 664.5 ms | **41.2 ms** | **16.1×** | 12.9–23.0 | <0.0001 |
| `/items?limit=50&sortBy=CommunityRating` | 647.2 ms | **40.9 ms** | **15.8×** | 14.2–19.5 | <0.0001 |
| `/items?limit=50&sortBy=Runtime` | 573.3 ms | **40.5 ms** | **14.2×** | 13.6–25.9 | <0.0001 |
| `/items?limit=50&sortBy=PremiereDate` | 564.5 ms | **50.4 ms** | **11.2×** | 9.4–21.8 | <0.0001 |

The endpoint gain (11–16×) is smaller than the raw SQL gain (103–6266×) because
roughly 40 ms is row-decode plus JSON serialisation for 50 items, which no index
can remove. That floor is why end-to-end measurement was necessary.

### ⚠ Result identity: the index ALONE is *not* result-identical

This is the part that decides whether the change is shippable, and it needs
stating plainly. Comparing the actual response bodies before and after:

| Sort | Same id **set**? | Ids differing | Distinct sort-key values in the 50-row window |
|---|---|---|---|
| `PremiereDate` | ✅ yes | 0 / 50 | 42 |
| `DigitalReleaseDate` | ✅ yes | 0 / 50 | 42 |
| `Runtime` | ❌ no | 1 / 50 | 42 |
| `CommunityRating` | ❌ no | **25 / 50** | **1 — every row is rated 10.0** |

The **sort-key values are identical in order in all four cases**, so the sort is
correct; what differs is purely which rows are chosen *within a tie group*.

`CommunityRating` is the pathological case: **91 items share the rating 10.0**
and the page shows 50, so "the top 50 by rating" is an arbitrary 50-of-91. The
current `ORDER BY` has no tiebreaker, so which 50 is unspecified. Adding an index
picks a different — equally valid — 50.

Today's ordering *appears* stable (three consecutive production calls returned
identical ids), but that stability is accidental: it comes from SQLite's physical
scan order on an unchanged table, not from anything the query specifies. It would
reshuffle on inserts, updates, a `VACUUM`, or any plan change.

**Therefore the index must not be shipped alone.** It must ship with the same
`, id` tiebreaker already applied to `DateCreated`, which:

* makes the ordering deterministic and specified rather than accidental,
* keeps the index usable (the tiebreaker direction must match the sort direction),
* and fixes the latent pagination bug whereby a client paging through
  rating-sorted results can currently see an item twice or miss it entirely.

### Risk assessment

| Dimension | Assessment |
|---|---|
| **Result identity — index alone** | ⚠ **Not identical.** Measured: `PremiereDate` and `DigitalReleaseDate` return the same id set (0/50 differ); `Runtime` differs by 1/50; `CommunityRating` differs by **25/50**, because 91 items tie at rating 10.0 and the page shows 50. The *sort-key order is identical* in every case — only tie selection moves. |
| **Result identity — index + `, id` tiebreaker (the actual proposal)** | Ordering becomes **deterministic and specified**. It still differs from today's *arbitrary* order at tie boundaries, exactly as the approved `DateCreated` fix did. This is a strict improvement: today's stability is accidental (physical scan order) and would break on any insert, update, `VACUUM`, or plan change. |
| **Pagination correctness** | Currently a client paging rating-sorted results can see an item twice or skip it, because the tie order is unspecified. The tiebreaker fixes this. |
| **Write cost** | Four more b-tree indexes on `media`. The table already carries ~17. Inserts/updates pay a small per-row cost, confined to library scans (batched), not the request path. |
| **Disk** | Four indexes over 1.33M rows. The two `DateCreated` indexes built in ~0.42 s over 290k rows; expect a few seconds each here, one time, at first startup after upgrade. |
| **Migration risk** | `CREATE INDEX IF NOT EXISTS` is additive and idempotent. No data is modified. Fully reversible with `DROP INDEX`. |
| **Blast radius if wrong** | Low. Worst case the planner ignores an index and performance is unchanged. |
| **Residual unknown** | End-to-end endpoint gain must be confirmed — SQL-level wins do not always survive (see the reverted `EXISTS`→`IN`). Do not ship on the SQL numbers alone. |

### Proposed change (for review — not applied)

A migration adding, and a matching `, id` tiebreaker for each sort in
`get_by_filter`:

```sql
CREATE INDEX IF NOT EXISTS idx_media_premiere_id
    ON media(COALESCE(released_at, digital_released_at), id);
CREATE INDEX IF NOT EXISTS idx_media_rating_id
    ON media(COALESCE(rating_audience, rating_critic), id);
CREATE INDEX IF NOT EXISTS idx_media_digital_id
    ON media(COALESCE(digital_released_at, released_at), id);
CREATE INDEX IF NOT EXISTS idx_media_runtime_id
    ON media(COALESCE(runtime, 0), id);
```

## Still to investigate

| Endpoint | Median | Note |
|---|---|---|
| `/items?sortBy=Random` | 388.5 ms | `ORDER BY RANDOM()` cannot use an index by nature. Needs a different technique, and any change alters *which* random row is returned — needs a product call on whether that matters. |
| `/items/languages` | 343.6 ms | Not yet root-caused. |
| `/items/certifications` | 288.4 ms | Not yet root-caused. |
| `/items/{id}/similar` | 292.9 ms | Not yet root-caused; "More Like This" appears on every item page. |
