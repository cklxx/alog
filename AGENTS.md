# alog — Agent Contract

Project caveats and hard gates only. Generic Rust / SQLite / git knowledge is
absent by design, and so is anything readable off the file tree. Match the
surrounding code's idiom, naming, and comment density.

`AGENTS.md` is canonical; `CLAUDE.md` is a symlink to it.

## Project shape

One binary, 2.6 MB, no runtime. `src/` is five files and no `lib.rs`:

| File | Owns |
|---|---|
| `extract.rs` | jsonl → `Row`. Claude Code and Codex CLI. No SQLite, no threads. |
| `store.rs` | DDL, `scan_file`, `apply`, `read_record`. One writer. |
| `query.rs` | Everything an agent or the viewer asks. `KEEP`, `TURNS`, `preview`. |
| `serve.rs` | tiny_http + `include_str!("viewer.html")`. |
| `main.rs` | CLI, and the rayon-scan / serial-write pipeline. |

## Hard gates

**Source files are read-only.** `.jsonl` under a user's session directory is
never written, moved, or locked. The index is derived and disposable; if a
choice trades that away, it is the wrong choice.

**Silent wrongness is a defect, not a tradeoff.** Every past bug in this repo
was silent: `read_record` verified `crc32` while the writer stored
`rec_hash` (crc32 | len<<32) so every search snippet came back empty; a naive
`sum(out_tok)` inflated tokens 1.63× because one LLM call copies its usage onto
every content block; a stray `}` in `viewer.html` blanked the page with no
request logged; and change detection trusted a byte length, so a file rewritten
in place to the same length left 100% of its rows answering for text on no disk
anywhere, with `integrity_check` still ok. Prefer a loud reject to a plausible
row.

**The index is a function of the files, and nothing else.** A cursor is trusted
only after the last indexed record is re-read and re-hashed; a `run` row whose
file is gone is deleted; symlinks are skipped; `sid` is assigned in sorted path
order, never by scan completion order. No metadata check may stand in front of
that re-hash — `mtime` is as restorable as the length, and an mtime fast-path
let a same-size rewrite with mtime restored to the same nanosecond return 4
stale hits and 0 real ones. Two builds of the same files must produce
the same `sid` — it is the `ev` primary key and appears in every `alog sql`
result, so a nondeterministic one silently redirects a cached `(sid, seq)`.
Anything that would let `sync` skip a changed file is the wrong optimization.

**A number in a comment, doc, or commit message is a claim.** Measure it or
delete it. Do not carry forward a figure you have not reproduced — six README
numbers were wrong when re-measured (ingest 41k→67k rec/s, errors 123→116 ms,
SQLite 3.51→3.50).

**`~/.claude/projects` and `~/.codex/sessions` are read-only in testing.**
Benchmark against an APFS clone: `cp -c -R ~/.claude/projects /tmp/alog-sandbox/`.

**Clean up `/tmp` after measuring.** Benchmark databases and corpus copies
accumulated to 141 GB and filled a 460 GB disk (2026-09-05). `df -h /` before a
full-corpus run, delete the artifacts after.

## Testing

CI runs macOS arm64, Linux x86_64, Linux arm64 and Windows x86_64, plus an MSRV
build. Windows is not decoration: it caught `ext_id` joining with the native
separator, which made the same session `proj/cc` on Unix and `proj\cc` there.
`scripts/e2e.sh` runs there under Git bash, where `python` cannot resolve an
MSYS `/tmp` path — so the script is bash-only, no python helpers.

`cargo test` covers the parts that fail silently: offset/crc round-trip, a
rewritten shorter file, ATTACH denial, the length guard in `rec_hash`,
path-derived session ids, `VACUUM INTO` under each connection kind, and
`viewer.html` parsing under bun (else node).

Adding a `viewer.html` edit means the parse test must still pass — `bun --check`
is wrong there, it executes the file and dies on `document`.

## Measured facts worth not rediscovering

- fts5 `content=''` costs 46.3% of text size; ordinary fts5 costs 172%.
  `contentless_delete=1` adds 1.7% and is required — a plain contentless table
  cannot DELETE, so a rewritten file leaves stale hits.
- Manual fts5 `merge`/`optimize` is harmful: 10× slower build, 68% larger, 5×
  slower queries.
- A per-file transaction costs 3.7 ms; 64 files per transaction turns 39 s of
  overhead into nothing.
- `PRAGMA query_only=ON` does not stop `ATTACH`, which creates the file. It also
  rejects `VACUUM INTO`, so `snapshot` uses a read-write handle.
- `cp` of a live WAL database loses every write still in the WAL, and
  `integrity_check` still returns `ok`.
- `F_FULLFSYNC` is 195× slower than `fsync` on Darwin (20.8 µs vs 4,049 µs).
- 44.2% of the corpus is framework bookkeeping. 84% of records have no `target`.
- Linux x86_64 ingests the same synthetic corpus at 153,585 rec/s against
  296,697 on macOS arm64, with a byte-identical index. 64 threads do not beat
  14: the scan saturates and the single writer serializes. Compare in MB/s, not
  rec/s -- a synthetic record is 232 bytes against 2,030 in the real corpus.
- The tail-record re-read costs 0.5 s over an unchanged 10,470-file corpus, and
  a plain `stat` of all of them is 46 ms. That 0.45 s buys the only proof that a
  cursor still belongs to the same file, and there is no cheaper proof: size and
  mtime are both restorable by the writer.
- Sharded fts5 builds 4.36× faster but bm25 IDF is corpus-global: top-10 overlap
  with the correct answer measured 10–60%. Not an option.

## Not planned

A custom on-disk format. Skipping `tool_result` text (51.3% of searchable bytes,
and what agents most need). Multi-machine or TB-scale anything.
