"""python test_alog.py — asserts, no framework."""
import json
import os
import sqlite3
import tempfile

import alog
from alog.extract import claude, ts_ms

D = tempfile.mkdtemp()


def write(name, records):
    p = os.path.join(D, name)
    with open(p, "w") as f:
        for r in records:
            f.write(json.dumps(r) + "\n")
    return p


def rec(seq, text="hello world", tool=None, err=None):
    content = [{"type": "text", "text": text}]
    if tool:
        content = [{"type": "tool_use", "name": tool, "input": {"file_path": "/a/b.py"}}]
    if err is not None:
        content = [{"type": "tool_result", "content": text, "is_error": err}]
    return {
        "uuid": f"u{seq}", "type": "assistant", "timestamp": "2026-09-04T12:00:00.000Z",
        "message": {"role": "assistant", "model": "m1", "content": content,
                    "usage": {"input_tokens": 10, "output_tokens": 5}},
    }


def test_ts():
    assert ts_ms("2026-09-04T12:00:00.000Z") == 1788523200000
    assert ts_ms("garbage") is None
    assert ts_ms(None) is None
    assert ts_ms(1788523200) == 1788523200000


def test_extract_edges():
    # usage with wrong types must not become a bogus integer
    o = {"message": {"role": "a", "usage": {"input_tokens": True, "output_tokens": "9"},
                     "content": "plain string"}}
    r = claude(o)
    assert r[6] is None and r[7] is None, "bool/str tokens must be None"
    assert r[10] == "plain string", "content as string must still be searchable"
    assert claude({})[10] == ""
    assert claude({"message": "not a dict"})[2] is None
    # a role nested too deep must not be picked up
    assert claude({"message": {"content": [{"message": {"role": "wrong"}}]}})[2] is None


def test_roundtrip_and_tail():
    p = write("s.jsonl", [rec(0), rec(1, tool="Bash"), rec(2, "boom", err=True)])
    con = alog.open_store(os.path.join(D, "i.db"))
    assert alog.sync_file(con, p, D) == 3
    assert alog.sync_file(con, p, D) == 0, "re-sync must be a no-op"

    with open(p, "a") as f:  # append while indexed: tail must pick it up
        f.write(json.dumps(rec(3, "later")) + "\n")
    assert alog.sync_file(con, p, D) == 1

    with open(p, "a") as f:  # partial line must be left alone
        f.write('{"uuid":"u4","type":"assis')
    assert alog.sync_file(con, p, D) == 0, "partial trailing line must not be indexed"

    raw = alog.read(con, 1, 0)
    assert json.loads(raw)["uuid"] == "u0", "read must return the original bytes"
    con.close()

    ro = alog.connect_ro(os.path.join(D, "i.db"))
    assert "records=4" in alog.catalog(ro)
    assert "Bash" in alog.query(ro, "SELECT tool FROM ev WHERE tool IS NOT NULL")
    assert "hits=1" in alog.search(ro, "boom")
    assert "ALOG_UNKNOWN_COLUMN" in alog.query(ro, "SELECT nope FROM ev")
    assert "did_you_mean" in alog.query(ro, "SELECT * FROM ev WHERE tool='Bashh'")
    assert "TRUNCATED" in alog.query(ro, "SELECT * FROM ev", limit=1)
    assert "seq" in alog.outline(ro, "s")
    assert "ALOG_UNKNOWN_SESSION" in alog.outline(ro, "nosuch")
    return ro


def test_attach_denied(ro):
    evil = os.path.join(D, "evil.db")
    out = alog.query(ro, f"ATTACH DATABASE '{evil}' AS e")
    assert "ALOG_DENIED" in out or "not authorized" in out, out
    assert not os.path.exists(evil), "ATTACH must not create a file"


def test_crc_detects_mutation():
    p = write("m.jsonl", [rec(0, "original")])
    con = alog.open_store(os.path.join(D, "m.db"))
    alog.sync_file(con, p, D)
    with open(p, "rb") as f:  # same length, different bytes
        b = f.read()
    with open(p, "wb") as f:
        f.write(b.replace(b"original", b"tampered"))
    try:
        alog.read(con, 1, 0)
        raise AssertionError("mutation must raise, never return the wrong record")
    except ValueError:
        pass
    con.close()


def test_dump_verify_redact():
    p = write("d.jsonl", [rec(0), rec(1, "api_key=sk-abcdefghijklmnopqrst")])
    con = alog.open_store(os.path.join(D, "d.db"))
    alog.sync_file(con, p, D)
    out = os.path.join(D, "d.jsonl.dump")
    with open(out, "w") as f:
        st = alog.dump(con, f)
    assert st["records"] == 2 and st["missing"] == 0
    assert alog.verify(out)["ok"], "byte-identical dump must verify"

    red = os.path.join(D, "r.dump")
    with open(red, "w") as f:
        st = alog.dump(con, f, redact=True)
    assert st["redactions"] >= 1
    assert "sk-abcdefghijklmnopqrst" not in open(red).read()
    assert alog.verify(red)["ok"]

    snap = os.path.join(D, "snap.db")
    alog.snapshot(con, snap)
    assert sqlite3.connect(snap).execute("SELECT count(*) FROM ev").fetchone()[0] == 2
    con.close()


def test_truncated_file_reindexes():
    p = write("t.jsonl", [rec(i) for i in range(5)])
    con = alog.open_store(os.path.join(D, "t.db"))
    assert alog.sync_file(con, p, D) == 5
    write("t.jsonl", [rec(0)])  # rewritten shorter: offsets are now invalid
    assert alog.sync_file(con, p, D) == 1
    assert con.execute("SELECT n_ev FROM run").fetchone()[0] == 1
    con.close()


if __name__ == "__main__":
    test_ts()
    test_extract_edges()
    ro = test_roundtrip_and_tail()
    test_attach_denied(ro)
    test_crc_detects_mutation()
    test_dump_verify_redact()
    test_truncated_file_reindexes()
    print("ok")
