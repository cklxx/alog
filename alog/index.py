import os
import sqlite3
import zlib

from .extract import FMT, ext_id, loads, sniff
from .schema import DDL, INDEXES

MAX_LINE = 32 << 20
try:
    from alogx import scan_file as _rust_scan
except ImportError:
    _rust_scan = None


def open_store(path):
    con = sqlite3.connect(path, isolation_level=None, timeout=30.0)
    con.execute("PRAGMA busy_timeout = 30000")
    con.executescript(DDL)
    return con


def scan(path, start, fmt):
    """Parse to the last complete line. A partial trailing line is left for the
    next pass, so tailing a file another process appends to is safe."""
    fn = FMT[fmt]
    rows, rejects, off = [], [], start
    with open(path, "rb") as f:
        f.seek(start)
        for line in f:
            n = len(line)
            if not line.endswith(b"\n"):
                break
            s = line.strip()
            if s:
                if n > MAX_LINE:
                    rejects.append((off, "too_large"))
                else:
                    try:
                        o = loads(s)
                    except Exception as e:
                        rejects.append((off, type(e).__name__))
                    else:
                        if isinstance(o, dict):
                            rows.append((off, n, zlib.crc32(s)) + fn(o))
                        else:
                            rejects.append((off, "not_object"))
            off += n
    return rows, rejects, off


def sync_file(con, path, root):
    st = os.stat(path)
    row = con.execute("SELECT sid, fmt, cursor, n_ev FROM run WHERE path=?", (path,)).fetchone()
    if row is None:
        with open(path, "rb") as f:
            head = f.readline().strip()
        try:
            h = loads(head) if head else None
        except Exception:
            h = None
        fmt = sniff(path, h if isinstance(h, dict) else None)
        sid = con.execute(
            "INSERT INTO run (ext, path, fmt) VALUES (?,?,?)", (ext_id(path, root), path, fmt)
        ).lastrowid
        cursor = n_ev = 0
    else:
        sid, fmt, cursor, n_ev = row
        if st.st_size == cursor:
            return 0
        if st.st_size < cursor:
            cursor = n_ev = 0

    rows, rejects, end = scan(path, cursor, fmt)
    if not rows and not rejects:
        return 0

    con.execute("BEGIN IMMEDIATE")
    try:
        if cursor == 0 and n_ev == 0:
            for t in ("ev", "reject"):
                con.execute(f"DELETE FROM {t} WHERE sid=?", (sid,))
            con.execute("DELETE FROM ftx WHERE rowid IN (SELECT rid FROM ftx_map WHERE sid=?)", (sid,))
            con.execute("DELETE FROM ftx_map WHERE sid=?", (sid,))
        con.executemany(
            "INSERT OR REPLACE INTO ev VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            [
                (sid, n_ev + i, off, ln, ts, kind, role, model, tool, target,
                 itk, otk, cr, err, crc)
                for i, (off, ln, crc, ts, kind, role, model, tool, target,
                        itk, otk, cr, err, _text) in enumerate(rows)
            ],
        )
        if rejects:
            con.executemany(
                "INSERT OR REPLACE INTO reject VALUES (?,?,?)", [(sid,) + x for x in rejects]
            )
        for i, r in enumerate(rows):
            text = r[-1]
            if text:
                rid = con.execute("INSERT INTO ftx(body) VALUES (?)", (text,)).lastrowid
                con.execute("INSERT INTO ftx_map VALUES (?,?,?)", (rid, sid, n_ev + i))
        if os.stat(path).st_size != st.st_size:
            con.execute("ROLLBACK")
            return sync_file(con, path, root)
        con.execute(
            "UPDATE run SET size=?, cursor=?, n_ev=? WHERE sid=?",
            (st.st_size, end, n_ev + len(rows), sid),
        )
        con.execute("COMMIT")
    except Exception:
        con.execute("ROLLBACK")
        raise
    return len(rows)


def sync(con, root):
    root = os.path.abspath(os.path.expanduser(root))
    files = records = 0
    for dp, dn, fn in os.walk(root):
        dn[:] = [d for d in dn if not d.startswith(".")]
        for f in fn:
            if f.endswith((".jsonl", ".ndjson")):
                n = sync_file(con, os.path.join(dp, f), root)
                if n:
                    files += 1
                    records += n
    return files, records


def add_index(con, name):
    con.execute(INDEXES[name])


def read(con, sid, seq):
    """Read original bytes. A crc mismatch is raised, never silently returned."""
    r = con.execute(
        "SELECT r.path, e.off, e.len, e.crc FROM ev e JOIN run r USING (sid)"
        " WHERE e.sid=? AND e.seq=?", (sid, seq)
    ).fetchone()
    if r is None:
        return None
    path, off, ln, crc = r
    with open(path, "rb") as f:
        f.seek(off)
        raw = f.read(ln)
    if zlib.crc32(raw.strip()) != crc:
        raise ValueError(f"{path}:{off} changed on disk; run sync")
    return raw
