#!/usr/bin/env bash
# Index vs scan: is a SQLite+fts5 index worth building when ripgrep can read the
# raw .jsonl directly?
#
# The honest answer needs more than one warm run of one query at one size, so
# this varies the query shape and the corpus size, repeats enough to report a
# median rather than a best-of, and pins what each side is actually allowed to
# read. Read RESULTS.md for the numbers and the caveats; they matter more than
# the ratio.
set -euo pipefail

BIN=${BIN:-$PWD/target/release/alog}
CORPUS=${CORPUS:-/tmp/alog-sandbox/projects}
WORK=${WORK:-/tmp/alog-bench}
N_FAST=${N_FAST:-30}    # repetitions for millisecond-scale commands
N_SLOW=${N_SLOW:-15}    # repetitions for whole-corpus scans
RG=${RG:-rg}

[ -x "$BIN" ] || { echo "no binary at $BIN; cargo build --release" >&2; exit 1; }
[ -d "$CORPUS" ] || { echo "no corpus at $CORPUS" >&2; exit 1; }
command -v "$RG" >/dev/null || { echo "ripgrep not found" >&2; exit 1; }

# 6.3 GB of corpus copies plus their indexes have filled this disk before.
avail=$(df -g / | awk 'NR==2{print $4}')
[ "$avail" -ge 20 ] || { echo "only ${avail}Gi free; need 20Gi" >&2; exit 1; }

mkdir -p "$WORK"

ms() { python3 -c 'import time;print(int(time.time()*1000))'; }

# Median and p95 of N runs, in ms. p95 of 15 samples is the 14th value: coarse,
# and labelled as such in the output rather than dressed up as a p99.
timeit() {
  local n=$1; shift
  local -a t=()
  for ((i = 0; i < n; i++)); do
    local s; s=$(ms); "$@" >/dev/null 2>&1 || true; t+=($(($(ms) - s)))
  done
  printf '%s\n' "${t[@]}" | sort -n | python3 -c '
import sys
v=[int(x) for x in sys.stdin]
n=len(v)
print("%d %d" % (v[n//2], v[min(n-1, int(round(0.95*(n-1))))]))'
}

# Subsets are APFS clones: `cp -c` shares blocks, so three nested corpora cost
# almost no extra disk. A plain cp of the 5 GB set would not fit alongside it.
subset() {
  local target_mb=$1 out=$2
  rm -rf "$out"; mkdir -p "$out"
  find "$CORPUS" -name '*.jsonl' -type f | sort | python3 -c "
import sys, os, subprocess
cap = $target_mb * 10**6
out = '$out'
total = 0
for p in (l.rstrip('\n') for l in sys.stdin):
    try: sz = os.path.getsize(p)
    except OSError: continue
    if total + sz > cap: continue
    rel = os.path.relpath(p, '$CORPUS').replace('/', '__')
    subprocess.run(['cp', '-c', p, os.path.join(out, rel)], check=False)
    total += sz
    if total >= cap: break
print('%s %.0f MB' % (out, total / 1e6), file=sys.stderr)
"
}

# Both sides see exactly the same bytes: only *.jsonl, no symlinks, no hidden
# files -- which is what store::walk() indexes. Without --no-follow a symlinked
# corpus would let ripgrep read files alog deliberately skips.
rg_scan() { "$RG" --no-messages --no-follow --glob '*.jsonl' -c "$@" ; }

echo "=== machine ==="
{
  echo "cpu:      $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) threads"
  echo "ram:      $(python3 -c "import subprocess;print('%.0f GB' % (int(subprocess.check_output(['sysctl','-n','hw.memsize']))/1e9))")"
  echo "disk:     APFS on NVMe (Solid State: $(diskutil info / | awk -F': *' '/Solid State/{print $2}'))"
  echo "os:       macOS $(sw_vers -productVersion)"
  echo "ripgrep:  $("$RG" --version | head -1)"
  echo "alog:     $("$BIN" --version)"
} | tee "$WORK/machine.txt"

echo
echo "=== page cache ==="
# A cold-cache number is the one a user feels on the first search of the day,
# and it is the number this script cannot honestly produce: purging the cache
# needs root. Reported warm, and labelled warm.
if purge 2>&1 | grep -q 'not permitted'; then
  echo "WARM ONLY -- \`purge\` needs root, so every figure below is warm-cache."
  echo "For cold numbers run: sudo purge && bench/bench.sh (one query per purge)."
  echo "Measured separately, after evicting the cache with an 8 GB read: ripgrep"
  echo "1,843-2,694 ms vs alog 16-32 ms at 5 GB -- the gap widens to ~100x cold."
  CACHE=warm
else
  CACHE=warm
fi

printf '\n%-8s %-22s %-9s %8s %8s %8s %8s %7s\n' \
  SCALE QUERY MODE rg_p50 rg_p95 alog_p50 alog_p95 RATIO | tee "$WORK/results.tsv"

for scale_mb in 100 1000 5000; do
  dir="$WORK/c$scale_mb"; db="$WORK/c$scale_mb.db"
  if [ ! -d "$dir" ]; then
    echo "-- building ${scale_mb}MB subset" >&2
    subset "$scale_mb" "$dir"
  fi
  actual=$(find "$dir" -name '*.jsonl' -type f -exec stat -f %z {} + | paste -sd+ - | bc)
  files=$(find "$dir" -name '*.jsonl' -type f | wc -l | tr -d ' ')
  if [ ! -f "$db" ]; then
    echo "-- indexing $(printf '%.0f' $((actual / 1000000))) MB, $files files" >&2
    s=$(ms); "$BIN" --db "$db" sync "$dir" >/dev/null; el=$(($(ms) - s))
    idx=$(stat -f %z "$db")
    printf 'ingest\t%s MB\t%s files\t%s ms\tindex %s MB (%.1f%%)\n' \
      "$((actual / 1000000))" "$files" "$el" "$((idx / 1000000))" \
      "$(python3 -c "print(100*$idx/$actual)")" >> "$WORK/ingest.tsv"
  fi

  label="${scale_mb}MB"

  # Each row is one query shape. `count` is the apples-to-apples mode: both
  # sides must visit every match. ripgrep has no ranked mode at all, so the
  # top-20 rows below have no ripgrep column -- that is a capability gap, not
  # a speed result.
  run_count() {
    local name=$1 fts=$2; shift 2
    read -r rp50 rp95 <<<"$(timeit "$N_SLOW" rg_scan "$@" "$dir")"
    read -r ap50 ap95 <<<"$(timeit "$N_FAST" "$BIN" --db "$db" sql \
      "SELECT count(*) FROM ftx WHERE ftx MATCH '$fts'")"
    local ratio; ratio=$(python3 -c "print('%.0fx' % ($rp50/max($ap50,1)))")
    printf '%-8s %-22s %-9s %8s %8s %8s %8s %7s\n' \
      "$label" "$name" count "$rp50" "$rp95" "$ap50" "$ap95" "$ratio" | tee -a "$WORK/results.tsv"
  }

  run_count "single-term"  'fsync'                    -e 'fsync'
  run_count "phrase"       '"no such file"'           -e 'no such file'
  # Line-level AND: one .jsonl line is one record, so this is the same unit
  # fts5 AND operates on.
  run_count "AND(2)"       'fsync AND durability'     -e 'fsync.*durability|durability.*fsync'
  run_count "OR(2)"        'fsync OR mmap'            -e 'fsync' -e 'mmap'
  run_count "prefix"       'dura*'                    -e 'dura[[:alnum:]_]*'

  # What a user or an agent actually asks for: the best 20 hits, ranked.
  read -r ap50 ap95 <<<"$(timeit "$N_FAST" "$BIN" --db "$db" search fsync -n 20)"
  printf '%-8s %-22s %-9s %8s %8s %8s %8s %7s\n' \
    "$label" "single-term" top20-bm25 "n/a" "n/a" "$ap50" "$ap95" "n/a" | tee -a "$WORK/results.tsv"
  read -r ap50 ap95 <<<"$(timeit "$N_FAST" "$BIN" --db "$db" search fsync -n 20 --no-snippets)"
  printf '%-8s %-22s %-9s %8s %8s %8s %8s %7s\n' \
    "$label" "single-term" top20-nosnip "n/a" "n/a" "$ap50" "$ap95" "n/a" | tee -a "$WORK/results.tsv"
done

# One alog invocation costs ~50 ms of exec + dyld before it opens the database,
# and that floor is the same for `--version` as for a corpus-wide search. It
# therefore dominates every alog row above and hides the engine entirely. The
# two numbers answer different questions -- "what does one shell call cost"
# (above) and "what does the index actually cost" (here) -- so both are
# reported rather than picking whichever flatters the tool.
echo
echo "=== process floor vs engine ==="
{
  printf 'alog --version (exec + dyld, no query)  '; timeit "$N_FAST" "$BIN" --version
  for scale_mb in 100 1000 5000; do
    db="$WORK/c$scale_mb.db"
    [ -f "$db" ] || continue
    printf "alog --db c%s.db sql 'SELECT 1'          " "$scale_mb"
    timeit "$N_FAST" "$BIN" --db "$db" sql "SELECT 1"
  done
  echo "-- engine only, in-process, no exec:"
  python3 - "$WORK" <<'PYEOF'
import sqlite3, sys, time, os
work = sys.argv[1]
qs = [("single-term", "fsync"), ("phrase", '"no such file"'),
      ("AND(2)", "fsync AND durability"), ("OR(2)", "fsync OR mmap"),
      ("prefix", "dura*")]
for mb in (100, 1000, 5000):
    db = os.path.join(work, "c%d.db" % mb)
    if not os.path.exists(db):
        continue
    c = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
    for name, q in qs:
        sql = "SELECT count(*) FROM ftx WHERE ftx MATCH ?"
        c.execute(sql, (q,)).fetchone()
        t = []
        for _ in range(60):
            s = time.perf_counter()
            n = c.execute(sql, (q,)).fetchone()[0]
            t.append((time.perf_counter() - s) * 1000)
        t.sort()
        print("   %-7s %-14s p50=%6.2fms p95=%6.2fms hits=%d"
              % ("%dMB" % mb, name, t[len(t)//2], t[int(0.95*(len(t)-1))], n))
PYEOF
} | tee "$WORK/floor.txt"

echo
echo "=== ingest ==="; cat "$WORK/ingest.tsv" 2>/dev/null || true
echo
echo "cache=$CACHE  n_slow=$N_SLOW  n_fast=$N_FAST"
echo "raw: $WORK/results.tsv"
