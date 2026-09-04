"""python -m alog.serve <dir> [--port 877] -- scan, then serve a viewer."""

import http.server
import json
import os
import socketserver
import tempfile
import threading
import urllib.parse
import webbrowser

from . import connect_ro, open_store, read, sync
from .query import _snip

HTML = os.path.join(os.path.dirname(__file__), "viewer.html")


class App(http.server.SimpleHTTPRequestHandler):
    db = None

    def log_message(self, *a):
        pass

    def _send(self, obj, code=200):
        b = json.dumps(obj, default=str).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        q = {k: v[0] for k, v in urllib.parse.parse_qs(u.query).items()}
        if u.path in ("/", "/index.html"):
            with open(HTML, "rb") as f:
                b = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
            return
        try:
            fn = getattr(self, "api_" + u.path.strip("/").replace("/", "_"), None)
            if fn is None:
                return self._send({"error": "not found"}, 404)
            self._send(fn(q))
        except Exception as e:
            self._send({"error": f"{type(e).__name__}: {e}"}, 500)

    def api_sessions(self, q):
        con = connect_ro(self.db)
        rows = con.execute(
            "SELECT r.ext, r.sid, r.n_ev, r.fmt, min(e.ts), max(e.ts),"
            " sum(e.is_err = 1), sum(coalesce(e.in_tok,0) + coalesce(e.out_tok,0)),"
            " count(DISTINCT e.tool)"
            " FROM run r LEFT JOIN ev e USING (sid) GROUP BY r.sid"
            " ORDER BY max(e.ts) DESC NULLS LAST LIMIT ?", (int(q.get("limit", 300)),)
        ).fetchall()
        return [
            {"ext": e, "sid": s, "n": n, "fmt": f, "t0": a, "t1": b,
             "err": er or 0, "tok": tk or 0, "tools": tl or 0}
            for e, s, n, f, a, b, er, tk, tl in rows
        ]

    def api_stats(self, q):
        con = connect_ro(self.db)
        n, runs, rej = con.execute(
            "SELECT (SELECT count(*) FROM ev), (SELECT count(*) FROM run),"
            " (SELECT count(*) FROM reject)"
        ).fetchone()
        return {
            "events": n, "sessions": runs, "rejects": rej,
            "bytes": con.execute("SELECT sum(size) FROM run").fetchone()[0] or 0,
            "tools": con.execute(
                "SELECT tool, count(*) FROM ev WHERE tool IS NOT NULL"
                " GROUP BY 1 ORDER BY 2 DESC LIMIT 12"
            ).fetchall(),
            "errors": con.execute(
                "SELECT tool, count(*) FROM ev WHERE is_err = 1"
                " GROUP BY 1 ORDER BY 2 DESC LIMIT 8"
            ).fetchall(),
            "models": con.execute(
                "SELECT model, count(*) FROM ev WHERE model IS NOT NULL"
                " GROUP BY 1 ORDER BY 2 DESC LIMIT 8"
            ).fetchall(),
        }

    def api_timeline(self, q):
        """Filtering is pushed into SQL: a 44k-event session can open with
        hundreds of framework bookkeeping records, and a client-side filter
        over a fixed window would show a blank screen."""
        con = connect_ro(self.db)
        sid = int(q["sid"])
        start, limit = int(q.get("start", 0)), int(q.get("limit", 400))
        keep = (
            "(is_err = 1 OR tool IS NOT NULL OR role IS NOT NULL"
            " OR kind IN ('assistant','user','system','result','failed','tool_result'))"
        )
        rows = con.execute(
            "SELECT seq, ts, kind, role, model, tool, target, is_err, len,"
            f" in_tok, out_tok, cache_r FROM ev WHERE sid = ? AND seq >= ? AND {keep}"
            " ORDER BY seq LIMIT ?", (sid, start, limit)
        ).fetchall()
        n_ev, shown = con.execute(
            f"SELECT n_ev, (SELECT count(*) FROM ev WHERE sid = ? AND {keep})"
            " FROM run WHERE sid = ?", (sid, sid)
        ).fetchone()
        return {
            "n_ev": n_ev, "n_shown": shown, "hidden": n_ev - shown,
            "rows": [
                {"seq": s, "ts": t, "kind": k, "role": r, "model": m, "tool": to,
                 "target": tg, "err": e, "len": ln, "in": i, "out": o, "cache": c}
                for s, t, k, r, m, to, tg, e, ln, i, o, c in rows
            ],
        }

    def api_record(self, q):
        con = connect_ro(self.db)
        try:
            raw = read(con, int(q["sid"]), int(q["seq"]))
        except ValueError as e:
            return {"error": str(e)}
        if raw is None:
            return {"error": "no such record"}
        try:
            return {"json": json.loads(raw)}
        except Exception:
            return {"raw": raw.decode("utf-8", "replace")}

    def api_search(self, q):
        con = connect_ro(self.db)
        term = q.get("q", "").strip()
        if not term:
            return {"hits": []}
        try:
            rows = con.execute(
                "SELECT r.ext, m.sid, m.seq, e.ts, e.kind, e.tool, e.target, e.is_err"
                " FROM ftx JOIN ftx_map m ON m.rid = ftx.rowid"
                " JOIN ev e ON e.sid = m.sid AND e.seq = m.seq"
                " JOIN run r ON r.sid = m.sid WHERE ftx MATCH ?"
                " ORDER BY rank LIMIT ?", (term, int(q.get("limit", 60)))
            ).fetchall()
        except Exception as e:
            return {"error": str(e)}
        words = [w for w in term.replace('"', " ").split() if w.upper() not in ("AND", "OR", "NOT")]
        out = []
        for ext, sid, seq, ts, kind, tool, target, err in rows:
            try:
                snip = _snip(read(con, sid, seq), words, 200)
            except Exception:
                snip = ""
            out.append({"ext": ext, "sid": sid, "seq": seq, "ts": ts, "kind": kind,
                        "tool": tool, "target": target, "err": err, "snip": snip})
        return {"hits": out}

    def api_errors(self, q):
        con = connect_ro(self.db)
        rows = con.execute(
            "SELECT r.ext, e.sid, e.seq, e.ts, e.tool, e.target FROM ev e"
            " JOIN run r USING (sid) WHERE e.is_err = 1 ORDER BY e.ts DESC LIMIT ?",
            (int(q.get("limit", 100)),)
        ).fetchall()
        out = []
        for ext, sid, seq, ts, tool, target in rows:
            try:
                raw = read(con, sid, seq)
                txt = raw.decode("utf-8", "replace") if raw else ""
            except Exception:
                txt = ""
            out.append({"ext": ext, "sid": sid, "seq": seq, "ts": ts, "tool": tool,
                        "target": target, "snip": txt[:400]})
        return {"rows": out}


def main(root, port=8877, db=None, no_open=False):
    db = db or os.path.join(tempfile.mkdtemp(), "alog.db")
    if not os.path.exists(db):
        con = open_store(db)
        print(f"scanning {root} ...", flush=True)
        files, records = sync(con, root)
        con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        con.close()
        print(f"{records:,} records from {files:,} files -> {db}")
    App.db = db
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.ThreadingTCPServer(("127.0.0.1", port), App) as srv:
        url = f"http://127.0.0.1:{port}"
        print(f"serving {url}  (ctrl-c to stop)")
        if not no_open:
            threading.Timer(0.4, lambda: webbrowser.open(url)).start()
        try:
            srv.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    import sys

    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)

    def opt(name, default=None):
        return a[a.index(name) + 1] if name in a else default

    main(a[0], int(opt("--port", 8877)), opt("--db"), "--no-open" in a)
