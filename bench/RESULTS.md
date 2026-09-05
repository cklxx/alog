# Index vs scan

Is a SQLite+fts5 index worth building when `ripgrep` can read the raw `.jsonl`
directly? Two tools in this space — [csift](https://github.com/wdhwg001/csift)
and [smc](https://github.com/scalecode-solutions/smc-cli-cc) — argue no, and
claim multi-GB regex scans in about a second with no index, no daemon, no
database. That is the right question to ask of alog, so this measures it.

Reproduce with `bash bench/bench.sh`. Raw output lands in `/tmp/alog-bench/`.

## Machine

| | |
|---|---|
| cpu | Apple M4 Pro, 14 threads |
| ram | 52 GB |
| disk | APFS on NVMe (Solid State: Yes) |
| os | macOS 26.3.1 |
| ripgrep | 15.1.0 |
| sqlite | 3.50.2, bundled in the binary |

## What is being compared

Both sides read exactly the same bytes: `*.jsonl` only, symlinks not followed —
which is what `store::walk()` indexes. Subsets are APFS clones (`cp -c`), so
three nested corpora share blocks instead of costing 6 GB each.

`count` is the apples-to-apples mode: both sides must visit every match.
ripgrep has no ranked mode at all, so the `top20-bm25` rows have no ripgrep
column — that is a capability gap, not a speed result.

15 repetitions for whole-corpus scans, 30 for millisecond commands, 60 for the
in-process engine measurements. p95 of 15 samples is the 14th value: coarse, and
labelled as such rather than dressed up as a p99.

## Per-invocation, warm cache

What a shell call costs, end to end. ms.

| scale | query | rg p50 | rg p95 | alog p50 | alog p95 | ratio |
|---|---|---|---|---|---|---|
| 100 MB | single-term | 102 | 124 | 57 | 98 | 2× |
| 100 MB | phrase | 96 | 110 | 53 | 105 | 2× |
| 100 MB | AND(2) | 94 | 107 | 52 | 96 | 2× |
| 100 MB | OR(2) | 117 | 129 | 55 | 91 | 2× |
| 100 MB | prefix | 96 | 116 | 54 | 89 | 2× |
| 1 GB | single-term | 152 | 177 | 52 | 56 | 3× |
| 1 GB | phrase | 142 | 150 | 53 | 61 | 3× |
| 1 GB | AND(2) | 172 | 193 | 53 | 62 | 3× |
| 1 GB | OR(2) | 173 | 185 | 52 | 67 | 3× |
| 1 GB | prefix | 175 | 200 | 56 | 83 | 3× |
| 5 GB | single-term | 501 | 626 | 51 | 54 | **10×** |
| 5 GB | phrase | 423 | 474 | 55 | 62 | 8× |
| 5 GB | AND(2) | 444 | 469 | 52 | 61 | 9× |
| 5 GB | OR(2) | 470 | 494 | 51 | 58 | 9× |
| 5 GB | prefix | 431 | 458 | 52 | 55 | 8× |

## The floor that table is hiding

alog's p50 is flat at 51–57 ms at every scale and for every query shape. That
is not query time. `alog --version`, which opens nothing, costs **49 ms**; `sql
'SELECT 1'` against the 5 GB index costs **51 ms**. Process startup — exec plus
dyld — is ~50 ms and swamps everything else.

Measured in-process, with no exec, the engine is:

| query | 100 MB | 1 GB | 5 GB | hits at 5 GB |
|---|---|---|---|---|
| single-term | 0.01 ms | 0.02 ms | **0.02 ms** | 292 |
| AND(2) | 0.02 | 0.03 | 0.04 | 121 |
| OR(2) | 0.02 | 0.04 | 0.08 | 2,224 |
| prefix | 0.01 | 0.10 | 0.38 | 7,791 |
| phrase | 0.07 | 0.42 | 1.94 | 3,830 |

**This is the actual result.** ripgrep grows linearly with corpus size — 102 →
152 → 501 ms for 50× more data. The index does not: single-term is 0.02 ms at
every scale, because an fts5 lookup is proportional to the number of *hits*, not
the number of bytes. Phrase and prefix do grow, because they touch more
postings, but 50× the data costs 28× on phrase and 38× on prefix.

So the ratio to quote depends on what is being claimed:

- **10× at 5 GB per shell invocation** — honest, and what a user feels today.
- **~25,000× on engine time** (501 ms vs 0.02 ms) — also true, and what matters
  for anything issuing many queries in one process, which is the case a library
  or a long-lived server would hit. Treat the exact figure as a floor estimate:
  0.02 ms is near this timer's resolution.

Neither is 250×. An earlier draft of this comparison claimed that, and it was
wrong twice over: it let ripgrep scan 11,322 non-`.jsonl` files (1.0 GB alog
never indexes, ~5× unfair), and its first "warm" run was still filling the page
cache. Fixing the glob and repeating properly moved 3.2 s down to 501 ms.

## Ingest, the cost side of the trade

| corpus | files | ingest | index | ratio |
|---|---|---|---|---|
| 99 MB | 1,135 | 0.67 s | 10 MB | 10.7% |
| 999 MB | 1,993 | 6.3 s | 129 MB | 12.9% |
| 4,999 MB | 9,762 | 39.8 s | 670 MB | 13.4% |

Ingest is linear at ~126 MB/s. At 5 GB the index saves 450 ms per query
(501 → 51), so it repays its 39.8 s build after **88 queries** — or immediately
if what is wanted is ranking rather than a match count, which ripgrep cannot do
at any price.

## Caveats

**The table is warm-cache.** Dropping the page cache on macOS needs root, so
`bench.sh` cannot honestly produce a cold number and does not pretend to; it says
so at runtime. To measure it properly: `sudo purge` before a single query, once
per query.

Cold-cache direction was checked with a weaker proxy — read an 8 GB file to push
the corpus out of cache, then time one query each, twice:

| | after eviction | warm |
|---|---|---|
| ripgrep, 5 GB | 2,694 / 1,843 ms | 501 ms |
| alog, 5 GB | 32 / 16 ms | 51 ms |

So the gap **widens to roughly 100×**, not narrows: ripgrep must pull 5 GB
through the disk while alog touches a few pages of a 670 MB index. The warm
table understates the index's advantage rather than flattering it. This proxy is
not a purge and the two samples are not a distribution — it establishes the
direction, not a figure to quote.

**ripgrep is not doing the same work.** It counts matching lines. alog returns
ranked results with bm25, filters by tool/error/time in SQL, and reads a snippet
back from the source at a byte offset. The `count` rows exist to compare the one
operation both can do.

**One machine, one filesystem.** APFS on NVMe, and `fsync` on Darwin is 195×
cheaper than `F_FULLFSYNC`. Nothing here transfers to a spinning disk, a network
filesystem, or Linux without re-measuring; the ingest figure especially.

**The corpus is one developer's real sessions**, so match counts reflect its
vocabulary. `fsync` has 292 hits in 5 GB; a term with 100,000 hits would move
the engine numbers up, and the single-term row is fast partly *because* the term
is rare. That is the common case for a search, but it is not the worst case.
