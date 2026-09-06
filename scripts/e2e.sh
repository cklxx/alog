#!/usr/bin/env bash
# End-to-end: real files on disk through sync, then every read command against
# the index they produced. Unit tests cover the library; this covers the CLI
# contract, which is what an agent and a user actually touch.
set -euo pipefail

BIN=${BIN:-target/release/alog}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
DB="$WORK/i.db"
SRC="$WORK/src"
mkdir -p "$SRC/proj"

fail() { echo "FAIL: $*" >&2; exit 1; }
has()  { grep -q -- "$2" <<<"$1" || fail "$3"; }

# --- Claude Code shape -------------------------------------------------------
cat > "$SRC/proj/cc.jsonl" <<'EOF'
{"uuid":"u0","type":"user","timestamp":"2026-09-05T10:00:00.000Z","message":{"role":"user","content":"index my sessions"}}
{"uuid":"u1","type":"assistant","timestamp":"2026-09-05T10:00:01.000Z","message":{"role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"reading the file now"}],"usage":{"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":3}}}
{"uuid":"u2","type":"assistant","timestamp":"2026-09-05T10:00:02.000Z","message":{"role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","name":"Bash","input":{"command":"grep -c fsync durability.md"}}],"usage":{"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":3}}}
{"uuid":"u3","type":"user","timestamp":"2026-09-05T10:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","content":"grep: durability.md: No such file","is_error":true}]}}
{"type":"attachment","timestamp":"2026-09-05T10:00:04.000Z"}
EOF

# --- Codex CLI shape ---------------------------------------------------------
cat > "$SRC/proj/cx.jsonl" <<'EOF'
{"timestamp":"2026-09-05T11:00:00.000Z","type":"session_meta","payload":{"id":"s1","cwd":"/tmp/demo"}}
{"timestamp":"2026-09-05T11:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"build it"}}
{"timestamp":"2026-09-05T11:00:02.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}","call_id":"c1"}}
{"timestamp":"2026-09-05T11:00:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"error: could not compile\nProcess exited with code 101"}}
{"timestamp":"2026-09-05T11:00:04.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":90,"output_tokens":12,"cached_input_tokens":64}}}}
{"timestamp":"2026-09-05T11:00:05.000Z","type":"event_msg","payload":{"type":"agent_message","message":"the build failed on a missing symbol"}}
EOF

echo "== sync =="
out=$("$BIN" --db "$DB" sync "$SRC")
has "$out" "11 records from 2 files" "expected 11 records, got: $out"

echo "== sync is incremental =="
out=$("$BIN" --db "$DB" sync "$SRC")
has "$out" "0 records from 0 files" "re-sync must do nothing, got: $out"

echo "== append is picked up =="
echo '{"uuid":"u4","type":"assistant","timestamp":"2026-09-05T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}' >> "$SRC/proj/cc.jsonl"
out=$("$BIN" --db "$DB" sync "$SRC")
has "$out" "1 records from 1 files" "append must add 1 record, got: $out"

echo "== stats =="
out=$("$BIN" --db "$DB" stats)
has "$out" "events    12" "expected 12 events, got: $out"
has "$out" "rejects   0"  "expected no rejects, got: $out"

echo "== catalog names both formats =="
out=$("$BIN" --db "$DB" catalog)
has "$out" "assistant" "catalog missing Claude Code kinds"
has "$out" "response_item/function_call" "catalog missing Codex kinds"
has "$out" "Bash" "catalog missing tool from Claude Code"
has "$out" "exec_command" "catalog missing tool from Codex"

echo "== search reaches both formats =="
has "$("$BIN" --db "$DB" search fsync)" "durability.md" "search missed Claude Code text"
has "$("$BIN" --db "$DB" search symbol)" "missing symbol" "search missed Codex text"
has "$("$BIN" --db "$DB" search '"no such file"')" "grep" "phrase search failed"

echo "== errors attribute the tool =="
out=$("$BIN" --db "$DB" errors)
has "$out" "Bash" "error row lost the Claude Code tool name"
has "$out" "exec_command" "error row lost the Codex tool name"

echo "== turns, both formats =="
out=$("$BIN" --db "$DB" turns proj/cc)
has "$out" "index my sessions" "Claude Code turn lost the prompt text"
out=$("$BIN" --db "$DB" turns proj/cx)
has "$out" "build it" "Codex turn lost the prompt text"
has "$out" "90" "Codex turn lost its token count"

echo "== show fills the what column =="
has "$("$BIN" --db "$DB" show proj/cc)" "reading the file now" "outline row was blank"

echo "== dump is byte-identical =="
"$BIN" --db "$DB" dump proj/cc > "$WORK/back.jsonl"
grep -v '^[[:space:]]*$' "$SRC/proj/cc.jsonl" > "$WORK/want.jsonl"
diff -q "$WORK/want.jsonl" "$WORK/back.jsonl" || fail "dump is not byte-identical"

echo "== sql, and its errors are machine-applicable =="
has "$("$BIN" --db "$DB" sql 'SELECT count(*) FROM ev')" "12" "sql count wrong"
has "$("$BIN" --db "$DB" sql 'SELECT toolz FROM ev')" "ALOG_UNKNOWN_COLUMN" "no typo candidate"
has "$("$BIN" --db "$DB" sql "SELECT sid FROM ev WHERE tool='Bashh'")" "did_you_mean" "no literal hint"
has "$("$BIN" --db "$DB" sql 'SELECT sid,seq FROM ev' -n 2)" "TRUNCATED" "truncation was silent"

echo "== the index is read-only to callers =="
"$BIN" --db "$DB" sql 'DELETE FROM ev' 2>&1 | grep -q readonly || fail "write was not refused"
"$BIN" --db "$DB" sql "ATTACH DATABASE '$WORK/evil.db' AS e" 2>&1 | grep -q ALOG_DENIED \
  || fail "ATTACH was not denied"
[ -e "$WORK/evil.db" ] && fail "ATTACH created a file"

echo "== secondary index, then snapshot =="
"$BIN" --db "$DB" index err >/dev/null
"$BIN" --db "$DB" snapshot "$WORK/snap.db" >/dev/null
has "$("$BIN" --db "$WORK/snap.db" stats)" "events    12" "snapshot lost rows"

echo "== a rewritten file is reindexed, not appended =="
head -2 "$SRC/proj/cc.jsonl" > "$WORK/t" && mv "$WORK/t" "$SRC/proj/cc.jsonl"
"$BIN" --db "$DB" sync "$SRC" >/dev/null
has "$("$BIN" --db "$DB" sql "SELECT n_ev FROM run WHERE ext='proj/cc'")" "2" \
  "shrunk file left stale rows"

echo "== a same-length in-place rewrite is detected =="
mkdir -p "$WORK/rw"
# gen <token> <file>: four same-length records, so a rewrite is byte-length
# identical. Written in bash: under Git bash on Windows, python cannot resolve
# the MSYS /tmp path this script works in.
gen() {
  : > "$2"
  for i in 0 1 2 3; do
    printf '{"uuid":"u%d","type":"assistant","timestamp":"2026-09-05T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"%s"}]}}\n' \
      "$i" "$1" >> "$2"
  done
}
gen alphaaaa "$WORK/rw/s.jsonl"
"$BIN" --db "$WORK/rw.db" sync "$WORK/rw" >/dev/null
before=$(wc -c < "$WORK/rw/s.jsonl")
gen omegabbb "$WORK/rw/s.jsonl"
[ "$before" = "$(wc -c < "$WORK/rw/s.jsonl")" ] || fail "the rewrite must keep the same length"
# Give the rewrite the mtime of another file, so neither length nor mtime has
# changed in a way a metadata shortcut could notice.
touch -r "$WORK/rw.db" "$WORK/rw/s.jsonl"
"$BIN" --db "$WORK/rw.db" sync "$WORK/rw" >/dev/null
has "$("$BIN" --db "$WORK/rw.db" search alphaaaa)" "hits=0" "stale text still answers after a same-length rewrite"
has "$("$BIN" --db "$WORK/rw.db" search omegabbb)" "hits=4" "the new text is not searchable"

echo "== a deleted file takes its rows with it =="
mkdir -p "$WORK/del"
cp "$SRC/proj/cx.jsonl" "$WORK/del/keep.jsonl"
cp "$SRC/proj/cx.jsonl" "$WORK/del/gone.jsonl"
"$BIN" --db "$WORK/del.db" sync "$WORK/del" >/dev/null
rm "$WORK/del/gone.jsonl"
has "$("$BIN" --db "$WORK/del.db" sync "$WORK/del")" "dropped 1 sessions" "deletion was not reported"
has "$("$BIN" --db "$WORK/del.db" sql "SELECT count(*) FROM run")" "1" "the run row for a deleted file survived"
has "$("$BIN" --db "$WORK/del.db" search "build it")" "hits=1" "the surviving session lost or kept too many hits"

echo "== sid is the same across two builds of the same files =="
"$BIN" --db "$WORK/s1.db" sync "$SRC" >/dev/null
"$BIN" --db "$WORK/s2.db" sync "$SRC" >/dev/null
a=$("$BIN" --db "$WORK/s1.db" sql "SELECT sid, ext FROM run ORDER BY sid")
b=$("$BIN" --db "$WORK/s2.db" sql "SELECT sid, ext FROM run ORDER BY sid")
[ "${a#*$'\n'}" = "${b#*$'\n'}" ] || fail "sid is not reproducible: $a vs $b"

echo "== a stale hit names the failure, it does not blank the column =="
mkdir -p "$WORK/st"
cat > "$WORK/st/s.jsonl" <<'EOF'
{"uuid":"u0","type":"assistant","timestamp":"2026-09-05T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"a staletoken lives here"}]}}
EOF
"$BIN" --db "$WORK/st.db" sync "$WORK/st" >/dev/null
# In-place edit, same length: tr, not python -- see the gen() note above.
tr 'a-z' 'A-Z' < "$WORK/st/s.jsonl" > "$WORK/st/up"
mv "$WORK/st/up" "$WORK/st/s.jsonl"
has "$("$BIN" --db "$WORK/st.db" search staletoken)" "changed on disk" "a stale snippet came back blank instead of named"
"$BIN" --db "$WORK/st.db" dump s >/dev/null 2>&1 && fail "dump must not exit 0 when records are unreadable"

echo "== search reports the total, not just that it truncated =="
mkdir -p "$WORK/many"
for i in 0 1 2 3 4; do
  printf '{"uuid":"u%d","type":"assistant","timestamp":"2026-09-05T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"widget recurs here"}]}}\n' "$i"
done > "$WORK/many/s.jsonl"
"$BIN" --db "$WORK/many.db" sync "$WORK/many" >/dev/null
out=$("$BIN" --db "$WORK/many.db" search widget -n 2)
has "$out" "hits=2 of 5" "a truncated search must report the total, got: $(head -1 <<<"$out")"
has "$("$BIN" --db "$WORK/many.db" search widget -n 9)" "hits=5 " \
  "an untruncated search must not print a total"

echo "== mcp speaks JSON-RPC over stdio =="
mcp_out=$({
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"fsync","limit":1}}}'
  echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"sql","arguments":{"query":"DELETE FROM ev"}}}'
  echo '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nosuchtool","arguments":{}}}'
  echo '{"jsonrpc":"2.0","id":6,"method":"bogus/method"}'
} | "$BIN" --db "$DB" mcp)
# Six requests carry an id; the notification does not and must draw no reply.
[ "$(grep -c . <<<"$mcp_out")" = "6" ] || fail "expected 6 responses, got $(grep -c . <<<"$mcp_out")"
grep -q 'notifications/initialized' <<<"$mcp_out" && fail "a notification must not be answered"
has "$mcp_out" '"protocolVersion"' "initialize must return a protocolVersion"
has "$mcp_out" '"name":"search"'   "tools/list must advertise search"
has "$mcp_out" '"name":"doctor"'   "tools/list must advertise doctor"
# A refused write is a failed call, not a success carrying an error body.
cat > "$WORK/mcpcheck.py" <<'PYEOF'
import json, sys
by = {}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    d = json.loads(line)          # every line must be valid JSON
    assert d["jsonrpc"] == "2.0", d
    by[d["id"]] = d
assert "tools" in by[2]["result"], "tools/list shape"
assert by[3]["result"].get("isError") is None, "a good search is not an error"
assert by[4]["result"].get("isError") is True, "a refused write must set isError"
assert by[5]["result"].get("isError") is True, "an unknown tool must set isError"
assert by[6]["error"]["code"] == -32601, "an unknown method is a protocol error"
for t in by[2]["result"]["tools"]:
    assert isinstance(t["inputSchema"], dict), t["name"]
    assert len(t["description"]) > 40, t["name"]
PYEOF
python3 "$WORK/mcpcheck.py" <<<"$mcp_out" || fail "mcp responses are not well-formed"

echo "== doctor sees what integrity_check cannot =="
mkdir -p "$WORK/doc"
for i in 0 1 2 3; do
  printf '{"uuid":"u%d","type":"assistant","timestamp":"2026-09-05T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"alphaaaa"}]}}\n' "$i"
done > "$WORK/doc/a.jsonl"
cp "$WORK/doc/a.jsonl" "$WORK/doc/b.jsonl"
"$BIN" --db "$WORK/doc.db" sync "$WORK/doc" >/dev/null
"$BIN" --db "$WORK/doc.db" doctor >/dev/null || fail "a fresh index must pass doctor"

# Same length, so only a content read can see it; plus a file that is gone.
sed 's/alphaaaa/omegabbb/g' "$WORK/doc/a.jsonl" > "$WORK/doc/t" && mv "$WORK/doc/t" "$WORK/doc/a.jsonl"
rm "$WORK/doc/b.jsonl"
# SQLite still calls this index perfectly healthy, which is the whole point.
has "$("$BIN" --db "$WORK/doc.db" sql 'PRAGMA integrity_check')" "ok" \
  "integrity_check should still say ok -- that is why doctor exists"
out=$("$BIN" --db "$WORK/doc.db" doctor 2>&1 || true)
"$BIN" --db "$WORK/doc.db" doctor >/dev/null 2>&1 && fail "doctor must exit non-zero on a stale index"
has "$out" "stale-record" "doctor missed a same-length rewrite"
has "$out" "file-gone"    "doctor missed a deleted source file"
j=$("$BIN" --json --db "$WORK/doc.db" doctor 2>&1 || true)
has "$j" '"ok":false' "doctor --json must report ok:false"
# sync repairs it, and doctor agrees.
"$BIN" --db "$WORK/doc.db" sync "$WORK/doc" >/dev/null
"$BIN" --db "$WORK/doc.db" doctor >/dev/null || fail "doctor must pass after sync heals the index"

echo "== --json errors carry kind, hint and retryable =="
j=$("$BIN" --json --db "$WORK/absent.db" stats 2>&1 >/dev/null || true)
has "$j" '"kind":"no-index"'   "missing index must classify as no-index, got: $j"
has "$j" '"retryable":false'   "no-index is not retryable"
has "$j" '"hint":'             "every error carries a hint"
j=$("$BIN" --json --db "$DB" nosuchcmd 2>&1 >/dev/null || true)
has "$j" '"kind":"bad-usage"'  "unknown command must classify as bad-usage, got: $j"
# The human path must stay human: one line, no JSON.
h=$("$BIN" --db "$WORK/absent.db" stats 2>&1 >/dev/null || true)
case "$h" in
  '{'*) fail "the non-json path must not emit JSON, got: $h" ;;
  "alog: "*) : ;;
  *) fail "expected 'alog: ...' on stderr, got: $h" ;;
esac

echo "== a closed pipe is an exit, not a panic =="
for cmd in "search fsync -n 50" "dump" "catalog" "errors"; do
  err=$("$BIN" --db "$DB" $cmd 2>&1 >/dev/null | head -3)
  case "$err" in
    *panicked*|*"Broken pipe"*) fail "$cmd wrote '$err' when the reader closed" ;;
  esac
done

echo "== no argument finds every framework directory under ~ =="
FAKE="$WORK/home"
mkdir -p "$FAKE/.claude/projects/p" "$FAKE/.codex/sessions/q"
cp "$SRC/proj/cc.jsonl" "$FAKE/.claude/projects/p/a.jsonl"
cp "$SRC/proj/cx.jsonl" "$FAKE/.codex/sessions/q/b.jsonl"
out=$(HOME="$FAKE" "$BIN" --db "$WORK/auto.db" sync)
has "$out" ".claude/projects" "auto-discovery missed the Claude Code directory"
has "$out" ".codex/sessions"  "auto-discovery missed the Codex directory"
out=$(HOME="$WORK/empty" "$BIN" --db "$WORK/none.db" sync 2>&1 || true)
has "$out" "no session directory" "empty home must say so, not succeed"

echo "== an unknown format is reported, not silently empty =="
mkdir -p "$WORK/other"
echo '{"lorem":"ipsum","dolor":[1,2,3]}' > "$WORK/other/x.jsonl"
out=$("$BIN" --db "$WORK/o.db" sync "$WORK/other" 2>&1 || true)
has "$out" "not searchable" "unknown format did not warn"

echo "ALL PASS"
