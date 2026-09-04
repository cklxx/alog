# alog

Agent trajectories, queryable. Your framework keeps writing its own `.jsonl`; alog builds a
SQLite index beside it so an agent can search millions of events in milliseconds.

```python
import alog

con = alog.open_store("index.db")
alog.sync(con, "~/.claude/projects")          # 1.3M records, 18 s

ro = alog.connect_ro("index.db")
print(alog.catalog(ro))                        # what's in here, ~370 tokens
print(alog.search(ro, '"connection refused"')) # 5 ms
print(alog.outline(ro, session_id))            # step-by-step, 9 tokens/step
```

- **Your files stay yours.** alog opens them read-only. Never writes, moves, or locks them.
- **The index is disposable.** `rm index.db` and re-sync. Nothing is lost but time.
- **It's just SQLite.** pandas, DuckDB, `sqlite3` CLI, `better-sqlite3` all read it as-is.

## Why not a directory of jsonl files

Those files work until you have 500 of them. Then:

| | jsonl | alog |
|---|---|---|
| find every session that touched `auth.py` | scan 6 GB | index lookup |
| "what did this command print last time" | `rg` across 10k files | 5 ms |
| hand one session to a colleague | copy a 300 MB file | filtered dump |
| what's even in here | `ls`, then guess | 367-token catalog |

The measured pain is not hypothetical: a Codex session directory reached 110 GiB with 97.6%
of one 1.7 GB file attributable to duplicated compaction snapshots
([codex#34268](https://github.com/openai/codex/issues/34268)); a 3.8 GB Claude Code session
file caused 12.8 GB RSS and a hang
([claude-code#22365](https://github.com/anthropics/claude-code/issues/22365)); forked session
transcripts duplicate 93–99% of their content
([claude-code#85179](https://github.com/anthropics/claude-code/issues/85179)).

## Measured

Apple M4 Pro, APFS/NVMe, SQLite 3.51.2, CPython 3.11.14. Corpus: 10,470 real Claude Code
session files, 5.4 GB.

| | |
|---|---|
| index size | **11.2%** of source (4.2% without full-text, 6.2% with two secondary indexes) |
| full-text index | **46.3%** of indexed text — contentless fts5 keeps no copy; ordinary fts5 is 172% |
| search | **0.4 ms** single term, 5 ms phrase, 29 ms prefix, over 573k documents |
| aggregate over 1.3M rows | **72 ms** (13 ms with the `tool` index built) |
| catalog for an agent | **367 tokens** |
| ingest | **74k records/s**, 121 MB/s, single process |
| steady-state append | 6,684 records/s, one commit each |

Numbers reproduce with `python -m alog.bench <dir>`.

## For agents

The consumer is usually a model with a limited context window, so:

- `catalog()` describes a 1.3M-record store in 367 tokens.
- Results **never silently truncate** — the header says `TRUNCATED at limit=50`.
- Errors name the fix: querying `toolz` returns
  `{"code":"ALOG_UNKNOWN_COLUMN","candidates":["tool","role","model"],"applicability":"MachineApplicable"}`.
- A wrong literal is caught too: `WHERE tool='BashTool'` returns
  `matched 0 rows. did_you_mean -> tool: Bash, BBash`.
- Raw SQL runs read-only under an authorizer that denies `ATTACH`. `PRAGMA query_only=ON` is
  not enough — under it `ATTACH` still succeeds and creates the file.

## Durability

`sync()` runs at `synchronous=NORMAL`. Committed records survive `kill -9` and OS panic; they
do **not** survive power loss. On this machine an honest barrier costs 195×: `fsync` 20.8 µs
vs `F_FULLFSYNC` 4,049 µs, which through SQLite is 70,175 ev/s vs 202 ev/s. The index is
rebuildable, so the trade is deliberate.

To copy a store, use `alog.snapshot(con, "backup.db")` — never `cp`. A `cp` of a live WAL
database loses every write still in the WAL and `PRAGMA integrity_check` still returns `ok`.

## Not for

- Multi-machine, multi-tenant, or TB-scale. This is one developer's laptop.
- Replaying the world an agent changed. alog stores what the agent did, not the filesystem
  it did it to.
- Being your source of truth. Your `.jsonl` files are.

## Install

```
pip install alog
```

Pure Python, stdlib `sqlite3` only. `orjson` is used if present (2.8× faster parsing) and not
required. A Rust scanner ships as an optional wheel (2138 MB/s vs 274 MB/s single-threaded);
without it everything still works.

## Roadmap

**v0 — now.** Index, search, outline, filtered dump, snapshot. Read-only over your files.

**v1.**
- Rust scanner as the default fast path, pure Python as the fallback. Measured 2138 MB/s at
  14 threads vs 809 MB/s for 8 Python processes; end-to-end gain is 1.12× because fts5 is
  70% of the pipeline.
- Differential CI: the Rust and Python extractors must produce byte-identical rows over a
  corpus plus an adversarial fixture set. Six divergences were found by synthetic cases that
  5.4 GB of real corpus never triggered, so the corpus alone is not a gate.
- Linux measurement. Every number here is macOS/APFS. `fsync` semantics differ on Linux, so
  the durability section is Darwin-only until measured.
- fts5 exact-duplicate collapse: index identical text once, keep every `(session, seq)` hit,
  so coverage is unchanged.

**v2.**
- A small state table agents coordinate through: claim, lease, fence, audit. Separate file at
  `synchronous=FULL` — it is authoritative and cannot be rebuilt, unlike the index. Sharing a
  file with the bulk indexer measured a work-claim p99 of 2,758 ms vs 1.34 ms split.
- Prefix ledger. 95.55% of a session's prompt tokens are a byte-repeat of the previous call
  and the median new tokens per call is 192, so `T_parent − cache_read` attributes every
  uncached prefix token to a cause using only integers the provider already wrote.
- TypeScript reader over the same SQLite schema, no native addon.

**Not planned.** Sharded fts5 (4.36× faster to build, but bm25 IDF is corpus-global: top-10
overlap with the correct answer measured 10–60%). Skipping `tool_result` text (half the bytes,
but it is what agents most need to search). Custom on-disk format.

## License

MIT
