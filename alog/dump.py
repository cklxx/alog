import hashlib
import json
import re

from .index import read

_KEY = re.compile(
    r"(api[_-]?key|secret|token|password|credential|authorization|bearer|private[_-]?key)", re.I
)
_VAL = [
    (re.compile(r"\bsk-[A-Za-z0-9_\-]{16,}"), "sk-REDACTED"),
    (re.compile(r"\bgh[pousr]_[A-Za-z0-9]{16,}"), "ghp_REDACTED"),
    (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), "AKIA_REDACTED"),
    (re.compile(r"\bxox[baprs]-[A-Za-z0-9\-]{10,}"), "xox-REDACTED"),
    (re.compile(r"eyJ[\w\-]{10,}\.[\w\-]{10,}\.[\w\-]{10,}"), "JWT_REDACTED"),
    (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
     "PRIVATE_KEY_REDACTED"),
]


def scrub(o, n=None):
    n = [0] if n is None else n
    if isinstance(o, dict):
        out = {}
        for k, v in o.items():
            if isinstance(k, str) and _KEY.search(k) and isinstance(v, (str, int)):
                out[k], n[0] = "REDACTED", n[0] + 1
            else:
                out[k] = scrub(v, n)
        return out
    if isinstance(o, list):
        return [scrub(x, n) for x in o]
    if isinstance(o, str):
        for pat, repl in _VAL:
            o, c = pat.subn(repl, o)
            n[0] += c
    return o


def snapshot(con, out):
    """VACUUM INTO, never cp: a cp of a live WAL db silently loses every
    uncommitted-to-main write and still returns integrity_check=ok."""
    con.execute("VACUUM INTO ?", (out,))
    return out


_FILTER = {
    "session": "r.ext", "kind": "e.kind", "role": "e.role",
    "tool": "e.tool", "target": "e.target", "is_err": "e.is_err",
}


def dump(con, out, session=None, since=None, until=None, kind=None, tool=None,
         is_err=None, match=None, redact=False, limit=None):
    """A filtered dump is the operation files cannot do."""
    where, params = ["1"], []
    if session:
        where.append("r.ext GLOB ?" if any(c in session for c in "*?") else "r.ext = ?")
        params.append(session)
    for col, op, v in (("e.ts", ">=", since), ("e.ts", "<=", until),
                       ("e.kind", "=", kind), ("e.tool", "=", tool)):
        if v is not None:
            where.append(f"{col} {op} ?")
            params.append(v)
    if is_err is not None:
        where.append("e.is_err = ?")
        params.append(1 if is_err else 0)
    if match:
        where.append(
            "(e.sid, e.seq) IN (SELECT m.sid, m.seq FROM ftx"
            " JOIN ftx_map m ON m.rid = ftx.rowid WHERE ftx MATCH ?)"
        )
        params.append(match)
    sql = (
        "SELECT r.ext, e.sid, e.seq FROM ev e JOIN run r USING (sid)"
        f" WHERE {' AND '.join(where)} ORDER BY r.ext, e.seq"
    )
    if limit:
        sql += f" LIMIT {int(limit)}"

    rows = con.execute(sql, params).fetchall()
    out.write(json.dumps({
        "kind": "alog.dump/1", "records": len(rows),
        "sessions": sorted({r[0] for r in rows}),
        "redacted": redact, "byte_identical": not redact,
    }, separators=(",", ":")) + "\n")

    h = hashlib.sha256()
    nred = missing = 0
    for ext, sid, seq in rows:
        try:
            raw = read(con, sid, seq)
        except (ValueError, OSError) as e:
            out.write(json.dumps({"_missing": {"session": ext, "seq": seq, "why": str(e)}}) + "\n")
            missing += 1
            continue
        line = raw.strip()
        if redact:
            try:
                n = [0]
                line = json.dumps(
                    scrub(json.loads(line), n), separators=(",", ":"), ensure_ascii=False
                ).encode()
                nred += n[0]
            except Exception:
                pass
        h.update(line + b"\n")
        out.write(line.decode("utf-8", "replace") + "\n")

    out.write(json.dumps({
        "_trailer": True, "written": len(rows) - missing,
        "missing": missing, "redactions": nred, "sha256": h.hexdigest(),
    }, separators=(",", ":")) + "\n")
    return {"records": len(rows) - missing, "missing": missing,
            "redactions": nred, "sha256": h.hexdigest()}


def verify(path):
    h = hashlib.sha256()
    trailer = None
    n = 0
    with open(path, encoding="utf-8") as f:
        for i, line in enumerate(f):
            s = line.rstrip("\n")
            if not s or i == 0:
                continue
            try:
                o = json.loads(s)
            except Exception:
                o = None
            if isinstance(o, dict):
                if o.get("_trailer"):
                    trailer = o
                    continue
                if "_missing" in o:
                    continue
            h.update(s.encode() + b"\n")
            n += 1
    if trailer is None:
        return {"ok": False, "why": "no trailer; dump is truncated"}
    ok = h.hexdigest() == trailer["sha256"] and n == trailer["written"]
    return {"ok": ok, "records": n, "expected": trailer["written"],
            "why": None if ok else "digest or count differs from trailer"}
