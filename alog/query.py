import difflib
import json
import re
import sqlite3
import time

COLS = "sid seq off len ts kind role model tool target in_tok out_tok cache_r is_err".split()


def connect_ro(path, deadline=5.0):
    """query_only=ON does NOT stop ATTACH: it succeeds and creates the file.
    Only an authorizer denying SQLITE_ATTACH does."""
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=10.0)
    con.execute("PRAGMA query_only = ON")
    con.set_authorizer(
        lambda op, *a: sqlite3.SQLITE_DENY
        if op in (sqlite3.SQLITE_ATTACH, sqlite3.SQLITE_DETACH)
        else sqlite3.SQLITE_OK
    )
    end = time.perf_counter() + deadline
    con.set_progress_handler(lambda: 1 if time.perf_counter() > end else 0, 20000)
    return con


def _ts(ms):
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(ms / 1000)) if ms else "?"


def _cell(v, cap=300):
    if v is None:
        return ""
    s = (v if isinstance(v, str) else str(v)).replace("\t", " ").replace("\n", "\\n")
    return s if len(s) <= cap else f"{s[:cap]}…(+{len(s) - cap}B)"


def catalog(con, top=25):
    """~294 tokens on a 2.68M-record store."""
    n = con.execute("SELECT count(*) FROM ev").fetchone()[0]
    runs, rej = (
        con.execute("SELECT (SELECT count(*) FROM run), (SELECT count(*) FROM reject)").fetchone()
    )
    out = [f"records={n} sessions={runs} rejects={rej}"]
    if not n:
        return out[0] + "\nempty; run alog sync <dir>"
    lo, hi = con.execute("SELECT min(ts), max(ts) FROM ev WHERE ts IS NOT NULL").fetchone()
    out.append(f"time: {_ts(lo)} .. {_ts(hi)}")
    out.append("cols: " + " ".join(COLS) + "   (ts epoch_ms)")
    probe = COLS[4:]
    nulls = con.execute(
        "SELECT " + ",".join(f"100.0*sum({c} IS NULL)/count(*)" for c in probe) + " FROM ev"
    ).fetchone()
    out.append("null%: " + " ".join(f"{c} {v:.0f}" for c, v in zip(probe, nulls)))
    for col in ("kind", "tool", "model"):
        g = con.execute(
            f"SELECT {col}, count(*) FROM ev WHERE {col} IS NOT NULL"
            f" GROUP BY 1 ORDER BY 2 DESC LIMIT {top}"
        ).fetchall()
        if g:
            tot = con.execute(f"SELECT count(DISTINCT {col}) FROM ev").fetchone()[0]
            out.append(f"{col}({tot}): " + " ".join(f"{k}:{v}" for k, v in g))
    return "\n".join(out)


def query(con, sql, limit=50):
    t0 = time.perf_counter()
    try:
        cur = con.execute(sql)
        rows = cur.fetchmany(limit + 1)
    except sqlite3.OperationalError as e:
        return _err(con, str(e))
    except sqlite3.DatabaseError as e:
        return json.dumps({"code": "ALOG_DENIED", "message": str(e)})
    names = [d[0] for d in cur.description] if cur.description else []
    more = len(rows) > limit
    rows = rows[:limit]
    head = f"rows={len(rows)} elapsed={(time.perf_counter() - t0) * 1000:.0f}ms"
    if more:
        head += f" TRUNCATED at limit={limit}, more rows exist"
    out = [head, "\t".join(names)] + ["\t".join(_cell(v) for v in r) for r in rows]
    if not rows:
        out.append(_hint(con, sql))
    return "\n".join(x for x in out if x)


def search(con, match, limit=20, read_snippet=None):
    """contentless fts5 stores no text, so highlight() returns NULL; the
    snippet comes from the source file via read_snippet if given."""
    t0 = time.perf_counter()
    try:
        rows = con.execute(
            "SELECT r.ext, m.sid, m.seq, e.ts, e.kind, e.tool, e.target FROM ftx"
            " JOIN ftx_map m ON m.rid = ftx.rowid"
            " JOIN ev e ON e.sid = m.sid AND e.seq = m.seq"
            " JOIN run r ON r.sid = m.sid"
            " WHERE ftx MATCH ? ORDER BY rank LIMIT ?", (match, limit + 1)
        ).fetchall()
    except sqlite3.OperationalError as e:
        return json.dumps({
            "code": "ALOG_BAD_QUERY", "message": str(e),
            "syntax": 'term, "exact phrase", A AND B, A OR B, NOT B, pre*',
        })
    more = len(rows) > limit
    rows = rows[:limit]
    head = f"hits={len(rows)} elapsed={(time.perf_counter() - t0) * 1000:.0f}ms"
    if more:
        head += f" TRUNCATED at limit={limit}"
    if not rows:
        return head + "\nno matches; try a shorter term or pre*"
    term = re.sub(r'[."*]|\b(AND|OR|NOT)\b', " ", match).split()
    out = [head, "session\tseq\twhen\tkind\ttool\ttarget" + ("\tmatch" if read_snippet else "")]
    for ext, sid, seq, ts, kind, tool, target in rows:
        cells = [_cell(ext, 60), str(seq), _ts(ts), _cell(kind, 20),
                 _cell(tool, 20), _cell(target, 60)]
        if read_snippet:
            cells.append(_cell(_snip(read_snippet(sid, seq), term), 240))
        out.append("\t".join(cells))
    return "\n".join(out)


def _snip(raw, terms, width=240):
    if not raw:
        return ""
    s = raw.decode("utf-8", "replace") if isinstance(raw, bytes) else raw
    low = s.lower()
    i = min((low.find(t.lower()) for t in terms if low.find(t.lower()) >= 0), default=-1)
    start = max(0, i - width // 3) if i >= 0 else 0
    return ("…" if start else "") + s[start:start + width]


def outline(con, ext, start=0, limit=80):
    """~9 tokens/event, so an 80-step window is ~720 tokens."""
    r = con.execute("SELECT sid, n_ev FROM run WHERE ext=?", (ext,)).fetchone()
    if r is None:
        near = difflib.get_close_matches(
            ext, [x[0] for x in con.execute("SELECT ext FROM run LIMIT 2000")], n=5, cutoff=0.3
        )
        return json.dumps({"code": "ALOG_UNKNOWN_SESSION", "message": ext, "candidates": near})
    sid, n_ev = r
    rows = con.execute(
        "SELECT seq, ts, kind, role, tool, target, is_err, len FROM ev"
        " WHERE sid=? AND seq>=? ORDER BY seq LIMIT ?", (sid, start, limit)
    ).fetchall()
    head = f"session={ext} events={n_ev}"
    if start + len(rows) < n_ev:
        head += f" (more: start={start + len(rows)})"
    out = [head, "seq\twhen\tkind\trole\ttool\ttarget\terr\tbytes"]
    for seq, ts, kind, role, tool, target, err, ln in rows:
        out.append("\t".join((
            str(seq), _ts(ts), _cell(kind, 24), _cell(role, 12),
            _cell(tool, 20), _cell(target, 70), "ERR" if err else "", str(ln),
        )))
    return "\n".join(out)


def _err(con, msg):
    low = msg.lower()
    code, cands = "ALOG_SQL_ERROR", []
    if "no such column" in low:
        code = "ALOG_UNKNOWN_COLUMN"
        cands = difflib.get_close_matches(msg.rsplit(":", 1)[-1].strip(), COLS, n=4, cutoff=0.4)
    elif "no such table" in low:
        code = "ALOG_UNKNOWN_TABLE"
        cands = [r[0] for r in con.execute("SELECT name FROM sqlite_master WHERE type='table'")]
    elif "interrupted" in low:
        code = "ALOG_TIMEOUT"
    return json.dumps({
        "code": code, "message": msg, "candidates": cands,
        "applicability": "MachineApplicable" if cands else "Unspecified",
    })


def _hint(con, sql):
    """Zero rows is usually a wrong literal, not an empty store."""
    hints = []
    for col, lit in re.findall(r"(\w+)\s*(?:=|LIKE)\s*'([^']{1,80})'", sql, re.I):
        if col not in COLS:
            continue
        vals = [
            str(r[0])
            for r in con.execute(f"SELECT DISTINCT {col} FROM ev WHERE {col} IS NOT NULL LIMIT 500")
        ]
        if lit not in vals:
            near = difflib.get_close_matches(lit, vals, n=3, cutoff=0.45)
            if near:
                hints.append(f"{col}: " + ", ".join(near))
    return "matched 0 rows. did_you_mean -> " + "; ".join(hints) if hints else ""
