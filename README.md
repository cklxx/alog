# alog

Claude Code and Codex CLI already write your sessions as `.jsonl`. alog indexes them in place
— read-only, never touching them — so 4.4M events answer in milliseconds.

```console
$ alog view
2769145 records from 10470 files in 42.5s (65084 rec/s)  ~/.claude/projects
1613204 records from  3151 files in 35.4s (45516 rec/s)  ~/.codex/sessions
serving http://127.0.0.1:8877
```

No path needed: `sync` and `view` find every framework's session directory under `~`.

One 2.6 MB binary. No runtime, no interpreter, no daemon.

- **Your files stay yours.** Opened read-only. Never written, moved, or locked.
- **The index is disposable.** Delete it, re-sync, lose nothing but time.
- **It's just SQLite.** pandas, DuckDB, `sqlite3`, `better-sqlite3` read it as-is.

## Measured

Apple M4 Pro, APFS/NVMe, SQLite 3.50.2 bundled. Corpus: **10,470 real Claude Code session
files, 5.62 GB, 2,769,145 records** — the full set, not a sample. Query latencies are the
median of 3 warm runs. A second column gives the same figures over the whole Codex CLI
corpus: **3,151 files, 3.73 GB, 1,613,204 records**.

| | Claude Code | Codex CLI |
|---|---|---|
| ingest | **65,084 rec/s** — 42.5 s, 14 threads | **45,516 rec/s** — 35.4 s |
| re-sync, nothing changed | **0.5 s** for 10,470 files | **0.3 s** for 3,151 files |
| parse failures | **0** of 2,769,145 | **0** of 1,613,204 |
| index size | **756 MB = 13.5%** of source | **650 MB = 17.4%** of source |
| full-text documents | 1,181,013 | 905,291 |
| single-term search | **2 ms** | **2 ms** |
| phrase search | **10 ms** | — |
| 50 newest failed tool calls | **110 ms**, **20 ms** after `alog index err` | **90 ms** of 9,239 |
| session timeline, 400 rows + previews | **20 ms** | **10 ms** |
| catalog for an agent | **1,669 bytes** | **1,570 bytes** |

One 2.6 MB binary, no runtime.

Reproduce with `alog sync <dir> && alog stats`. The index size is what `sync` writes; the
optional secondary indexes (`alog index err ts target`) add 134.5 MB on top, 17.6% more.
Search needs no index — fts5 is built during `sync`.

A re-sync of an unchanged corpus reads; it does not `stat`. Each file's last indexed record is
re-read and re-hashed, because no file metadata can see a file rewritten in place. That costs
0.5 s over 10,470 files and is why the index cannot go stale — see below.

### Linux

The numbers above are macOS. To compare platforms without moving anyone's sessions, the same
generator builds the same synthetic corpus on both machines — **800 files, 167 MB, 720,000
records**, and both produce an identical index: 720,000 rows, 18,400 errors, 720,000 full-text
documents, 97 MB.

| | macOS arm64 | Linux x86_64 |
|---|---|---|
| | M4 Pro, 14 threads, APFS | Xeon 8457C, 64 threads, ext4 |
| ingest | **296,697 rec/s** — 2.4 s | **153,585 rec/s** — 4.7 s |
| re-sync, nothing changed | 0.1 s | 0.0 s |
| single-term search | 90 ms | 173 ms |
| phrase search | 110 ms | 207 ms |
| 50 newest failed tool calls | 30 ms | 49 ms |
| binary | 2.60 MB | 3.10 MB |

Read the ratio, not the rec/s: a synthetic record is 232 bytes against 2,030 in the real
corpus, so these rates are per-record cheap and are not comparable to the table above. In
MB/s the same rows read 70 and 36, against 132 on real files.

More threads do not help: ingest is one SQLite writer behind a parallel scan, so the scan
saturates early and the write serializes. The Linux box was at load average 9.0 with other
tenants, so treat its latencies as an upper bound. The unit tests and the full end-to-end suite
pass there with no source change.

`fsync` is still unmeasured off Darwin, so the durability section below remains Darwin-only.

### Why an index at all

`ripgrep` can read the same files with no index, no daemon, no database — so the
index has to earn its 13.5%. Measured over the same `*.jsonl` files, warm cache,
median of 15 runs ([bench/RESULTS.md](bench/RESULTS.md), reproduce with
`bash bench/bench.sh`):

| corpus | ripgrep `-c` | alog | |
|---|---|---|---|
| 100 MB | 102 ms | 57 ms | 2× |
| 1 GB | 152 ms | 52 ms | 3× |
| 5 GB | 501 ms | 51 ms | **10×** |

ripgrep grows linearly; the index does not. And ~50 ms of alog's 51 is process
startup — `alog --version` costs 49 ms — so in-process the engine answers a
single-term query in **0.02 ms at every scale**, because an fts5 lookup is
proportional to hits, not bytes. Cold-cache the gap widens to ~100×, since
ripgrep must pull 5 GB through the disk.

Against a 39.8 s build for 5 GB, the index repays itself after 88 queries — or
on the first one that needs ranking, which `-c` cannot do at any price.

## Why not a directory of jsonl files

They work until you have 500. Then:

| | jsonl | alog |
|---|---|---|
| every session that touched `auth.py` | scan 5.6 GB | 170 ms scan of the index |
| what did this command print last time | `rg` across 10k files | 2 ms full-text |
| what broke, and in which tool | grep and hope | `alog errors`, 110 ms |
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
alog sync [<dir>...]        index sessions; no arg finds them under ~
alog view [<dir>...]        sync, then open the viewer
alog search <query>         full-text, fts5 syntax
alog show <session> [seq]   a session's timeline, or one raw record
alog sql <query>            read-only SQL over the index
alog turns <session>        one line per user request: work, cost, errors
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

## Staleness

A derived index is only useful if it cannot quietly disagree with its source. Four ways it
could, all measured on real files and all now closed:

- **A file rewritten in place to the same length.** A secret redacted, a transcript
  regenerated. `sync` used to skip it on a length comparison and report `0 records from 0
  files`, leaving 100% of its rows answering for text on no disk anywhere — and because fts5
  here is contentless, the index held no plaintext for an audit to find. Every count and
  `PRAGMA integrity_check` still returned ok.
- **A file rewritten longer.** The old cursor still pointed inside it, so half the index stayed
  stale: a query for text that existed returned half its hits, and a query for text that was
  gone returned the other half.
- **A deleted file.** Its `run` row and its documents survived, answering searches and skewing
  the corpus-global bm25 IDF that every other session's ranking depends on.
- **A symlink beside its target.** Followed, so one file was indexed twice at double weight.

`sync` now re-reads and re-hashes each file's last indexed record before trusting its cursor,
drops sessions whose files are gone, and skips symlinks. No metadata check stands in front of
that read: `mtime` is as restorable as the length — one `utime` call, which archivers and sync
tools make routinely — and with an mtime fast-path in place, a same-size rewrite whose mtime
was restored to the same nanosecond returned 4 stale hits and 0 real ones. `sid` is assigned in sorted path order
rather than by whichever thread finished first — it is the `ev` primary key and appears in
every `alog sql` result, and a nondeterministic one changed 99.9% of session ids between two
builds of the same files.

When a record cannot be verified, the tool says so. A stale search hit prints
`<...changed on disk; run \`alog sync\`>` in place of its snippet, and `dump` exits non-zero
rather than writing a short file and reporting success.

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
git clone https://github.com/cklxx/alog && cd alog && cargo build --release
```

Binary at `target/release/alog`; copy it anywhere on your `PATH`. CI also
uploads a build per platform on every push — macOS arm64, Linux x86_64, Linux
arm64, Windows x86_64.

Not on crates.io: the name `alog` there belongs to an unrelated crate.

## Roadmap

**v0.1 — now.** Rust core, SQLite index, full-text search, timeline, turns, error feed,
byte-identical dump, snapshot, viewer. **Claude Code and Codex CLI**, detected per record, not
per store — the two live side by side in one index. Any other `.jsonl` is still indexed and
stays dumpable, but its semantic columns come back NULL and `sync` says so.

**v0.2.**
- Released prebuilt binaries, and a crates.io name that is not already taken.
- OpenHands event streams, SWE-agent `.traj`, and OpenTelemetry GenAI spans.
- `fsync` measurement off Darwin. Ingest and query latency are now measured on Linux x86_64;
  the durability numbers are not, and `fsync` semantics differ, so that section stays
  Darwin-only until they are.
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

MIT — see [LICENSE](LICENSE).
