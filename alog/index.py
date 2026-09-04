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


def scan(path, start, fmt=None):
    """Parse to the last complete line. A partial trailing line is left for the
    next pass, so tailing a file another process appends to is safe.
    fmt=None sniffs from the first record, reusing this one open()."""
    rows, rejects, off, fn = [], [], start, FMT[fmt] if fmt else None
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
                            if fn is None:
                                fmt = sniff(path, o)
                                fn = FMT[fmt]
                            rows.append((off, n, zlib.crc32(s)) + fn(o))
                        else:
                            rejects.append((off, "not_object"))
            off += n
    return rows, rejects, off, fmt or "generic"


def sync_file(con, path, root, _tx=True):
    st = os.stat(path)
    row = con.execute("SELECT sid, fmt, cursor, n_ev FROM run WHERE path=?", (path,)).fetchone()
    if row is None:
        rows, rejects, end, fmt = scan(path, 0, None)
        sid = con.execute(
            "INSERT INTO run (ext, path, fmt) VALUES (?,?,?)", (ext_id(path, root), path, fmt)
        ).lastrowid
        cursor = n_ev = 0
        fresh = True
    else:
        sid, fmt, cursor, n_ev = row
        if st.st_size == cursor:
            return 0
        fresh = st.st_size < cursor
        if fresh:
            cursor = n_ev = 0
        rows, rejects, end, _ = scan(path, cursor, fmt)
    if not rows and not rejects:
        return 0

    if _tx:
        con.execute("BEGIN IMMEDIATE")
    try:
        if fresh and row is not None:
            for t in ("ev", "reject"):
                con.execute(f"DELETE FROM {t} WHERE sid=?", (sid,))
            con.execute(
                "DELETE FROM ftx WHERE rowid IN (SELECT rid FROM ftx_map WHERE sid=?)", (sid,)
            )
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
        # One executemany per table, not one execute per document: at 48k
        # records the per-execute path cost 2.98s of 9.1s.
        texts = [(n_ev + i, r[-1]) for i, r in enumerate(rows) if r[-1]]
        if texts:
            first = con.execute(
                "SELECT coalesce(max(rowid), 0) + 1 FROM ftx_map"
            ).fetchone()[0]
            con.executemany(
                "INSERT INTO ftx(rowid, body) VALUES (?,?)",
                [(first + j, t) for j, (_seq, t) in enumerate(texts)],
            )
            con.executemany(
                "INSERT INTO ftx_map VALUES (?,?,?)",
                [(first + j, sid, seq) for j, (seq, _t) in enumerate(texts)],
            )
        if os.stat(path).st_size != st.st_size:
            if _tx:
                con.execute("ROLLBACK")
            return sync_file(con, path, root, _tx)
        con.execute(
            "UPDATE run SET size=?, cursor=?, n_ev=? WHERE sid=?",
            (st.st_size, end, n_ev + len(rows), sid),
        )
        if _tx:
            con.execute("COMMIT")
    except Exception:
        if _tx:
            con.execute("ROLLBACK")
        raise
    return len(rows)


def sync(con, root, batch=64):
    """Commit `batch` files per transaction: per-file transaction overhead
    measured 3.7 ms, which over 10470 files is 39 s of the 420 s full run."""
    root = os.path.abspath(os.path.expanduser(root))
    paths = []
    for dp, dn, fn in os.walk(root):
        dn[:] = [d for d in dn if not d.startswith(".")]
        paths += [os.path.join(dp, f) for f in fn if f.endswith((".jsonl", ".ndjson"))]
    files = records = 0
    for i in range(0, len(paths), batch):
        con.execute("BEGIN IMMEDIATE")
        try:
            for p in paths[i : i + batch]:
                n = sync_file(con, p, root, _tx=False)
                if n:
                    files += 1
                    records += n
            con.execute("COMMIT")
        except Exception:
            con.execute("ROLLBACK")
            raise
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
