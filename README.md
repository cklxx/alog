# alog

Claude Code already writes your sessions as `.jsonl`. alog indexes them in place — read-only,
never touching them — so 2.7M events answer in milliseconds.

```console
$ alog view ~/.claude/projects
2769145 records from 10470 files in 41.6s (66556 rec/s)
serving http://127.0.0.1:8877
```

One 2.6 MB binary. No runtime, no interpreter, no build step.

- **Your files stay yours.** Opened read-only. Never written, moved, or locked.
- **The index is disposable.** Delete it, re-sync, lose nothing but time.
- **It's just SQLite.** pandas, DuckDB, `sqlite3`, `better-sqlite3` read it as-is.

## Measured

Apple M4 Pro, APFS/NVMe, SQLite 3.50.2 bundled. Corpus: **10,470 real Claude Code session
files, 5.62 GB, 2,769,145 records** — the full set, not a sample. Query latencies are the
median of 3 warm runs.

| | |
|---|---|
| ingest | **66,556 rec/s** — 41.6 s for 5.62 GB, 14 threads |
| re-sync, nothing changed | **0.6 s** for 10,470 files |
| parse failures | **0** of 2,769,145 |
| index size | **763 MB = 13.6%** of source |
| single-term search | **2 ms** over 1,181,013 documents |
| phrase search | **2 ms** |
| 50 newest failed tool calls | **116 ms**, or **17 ms** after `alog index err` |
| session timeline, 400 rows + previews | **28 ms** |
| catalog for an agent | **1,669 bytes** |
| binary | **2.6 MB**, no runtime |

Reproduce with `alog sync <dir> && alog stats`. The index size is what `sync` writes; the
optional secondary indexes (`alog index err ts target`) add 134.5 MB on top, 17.6% more.
Search needs no index — fts5 is built during `sync`.

## Why not a directory of jsonl files

They work until you have 500. Then:

| | jsonl | alog |
|---|---|---|
| every session that touched `auth.py` | scan 5.6 GB | 165 ms scan of the index |
| what did this command print last time | `rg` across 10k files | 2 ms full-text |
| what broke, and in which tool | grep and hope | `alog errors`, 116 ms |
| hand one session to a colleague | copy a 300 MB file | `alog dump <session>` |
| what's even in here | `ls`, then guess | 1,669-byte catalog |

The pain is documented, not hypothetical: a Codex session directory reached 110 GiB with
97.6% of one 1.7 GB file attributable to duplicated compaction snapshots
([codex#34268](https://github.com/openai/codex/issues/34268)); a 3.8 GB Claude Code session
file caused 12.8 GB RSS and a hang
([claude-code#22365](https://github.com/anthropics/claude-code/issues/22365)); forked
transcripts duplicate 93–99% of their content
([claude-code#85179](https://github.com/anthropics/claude-code/issues/85179)).

## Commands

```console
alog sync <dir>...          index .jsonl files, incrementally
alog view [<dir>]           scan if needed, then open the viewer
alog search <query>         full-text, fts5 syntax
alog show <session> [seq]   a session's timeline, or one raw record
alog sql <query>            read-only SQL over the index
alog catalog                what is in the store, in ~340 tokens
alog errors                 failed tool calls, newest first
alog dump [<session>]       byte-identical jsonl to stdout
alog index <col>            secondary index: kind tool target ts err
alog snapshot <out.db>      consistent copy (VACUUM INTO, never cp)
alog stats                  ingest and size numbers
```

## The viewer

`alog view` scans, serves, and opens a browser. Four views: a timeline grouped by day with
error rows tinted, a cross-session error feed naming which tool failed and on what, tool and
model distributions, and full-text search with highlighted matches. Click any row for the raw
JSON. A filter box narrows 10,470 sessions as you type.

Screen budget is the constraint. **44.2% of the corpus is framework bookkeeping**
(`attachment`, `mode`, `permission-mode`, `queue-operation` — 53.4% on the largest session), so
the timeline filters it in SQL and reports the count it hid; filtering in the browser over a
fixed window showed a blank screen on a session whose first hundreds of records are all
bookkeeping. Token counts appear only when non-zero, byte sizes only above 2 KB, full arguments
only on click.

Indexed columns alone leave rows blank, so each row falls back to a text preview read from the
source: without it **121 of 400 rows on a real session rendered as a bare role name** — a
`system` record has no `message.content` (its `subtype` is the only readable field), a redacted
`thinking` block leaves an empty string, and `TaskCreate`/`Agent`/`ToolSearch` carry everything
in an input object with no path-like target. With the fallbacks in place that count is 1.

## For agents

The reader is usually a model with a limited context window:

- `alog catalog` describes a 2.77M-record store in **1,669 bytes** — schema, time range,
  per-column null rate, and the full `kind`/`tool`/`model` distributions.
- Results **never silently truncate**: the header reads
  `rows=3 elapsed=9ms TRUNCATED at limit=3, more rows exist`.
- Errors name the fix: `SELECT toolz FROM ev` returns
  `{"code":"ALOG_UNKNOWN_COLUMN","candidates":["tool"],"applicability":"MachineApplicable"}`.
- A wrong literal too: `WHERE tool='Bashh'` returns
  `matched 0 rows. did_you_mean -> tool: Bash, BBash`.
- SQL runs read-only under an authorizer denying `ATTACH`. `PRAGMA query_only=ON` is not
  enough — under it `ATTACH` still succeeds and creates the file, handing an agent an
  arbitrary file-create primitive.

## Durability

`sync` runs at `synchronous=NORMAL`. Committed records survive `kill -9` and OS panic; they do
**not** survive power loss. An honest barrier costs 195× on this machine: `fsync` 20.8 µs vs
`F_FULLFSYNC` 4,049 µs, which through SQLite is 70,175 ev/s vs 202 ev/s. The index is
rebuildable from your files, so the trade is deliberate.

Copy a store with `alog snapshot`, never `cp`. A `cp` of a live WAL database loses every write
still in the WAL and `PRAGMA integrity_check` still returns `ok`. `snapshot` is `VACUUM INTO`,
which needs a read-write handle: `query_only=ON` rejects it, and a read-only handle cannot
create the `-shm` file a WAL database requires.

## Not for

- Multi-machine, multi-tenant, or TB-scale. This is one developer's laptop.
- Replaying the world an agent changed. alog stores what the agent did, not the filesystem it
  did it to.
- Being your source of truth. Your `.jsonl` files are.

## Install

```console
cargo install alog
```

Or build from source: `cargo build --release`, binary at `target/release/alog`.

## Roadmap

**v0.1 — now.** Rust core, SQLite index, full-text search, timeline, error feed, byte-identical
dump, snapshot, viewer. **Claude Code session files only.** Any `.jsonl` is indexed and every
record stays dumpable, but the semantic columns (`role`, `model`, `tool`, `target`, tokens) and
the full-text index are filled by a Claude Code extractor. Measured on 5 Codex CLI rollout
files: 48 records indexed, 0 full-text documents, and those columns 100% NULL — Codex nests its
content under `payload`, so it needs its own extractor.

**v0.2.**
- Prebuilt binaries for macOS, Linux (glibc and musl), Windows.
- A Codex CLI extractor, then OpenHands event streams, SWE-agent `.traj`, and OpenTelemetry
  GenAI spans. Format detection per file, not per store.
- Linux measurement. Every number here is macOS/APFS; `fsync` semantics differ on Linux, so
  the durability section is Darwin-only until measured.
- fts5 exact-duplicate collapse: index identical text once, keep every `(session, seq)` hit,
  so coverage is unchanged. fts5 tokenize+index is the single largest cost in `sync` —
  measured by ablation at 41% of ingest wall time in the Python prototype, not yet re-measured
  in Rust.

**v0.3.**
- A small state table agents coordinate through: claim, lease, fence, audit. Separate file at
  `synchronous=FULL` — it is authoritative and cannot be rebuilt, unlike the index. Sharing a
  file with the bulk indexer measured a work-claim p99 of 2,758 ms vs 1.34 ms split.
- Prefix ledger. 95.55% of a session's prompt tokens are a byte-repeat of the previous call
  and the median new tokens per call is 192, so `T_parent − cache_read` attributes every
  uncached prefix token to a cause using only integers the provider already wrote.
- A library crate, and language bindings over the same SQLite schema.

**Not planned.** Sharded fts5 (4.36× faster to build, but bm25 IDF is corpus-global: top-10
overlap with the correct answer measured 10–60%). Skipping `tool_result` text (51.3% of
searchable bytes, and it is what agents most need to search). A custom on-disk format.

## License

MIT
