"""python -m alog.bench <dir> [--files N]"""

import glob
import os
import sys
import tempfile
import time

from . import catalog, connect_ro, open_store, search, sync_file


def main(root, nfiles=0):
    root = os.path.abspath(os.path.expanduser(root))
    files = sorted(
        glob.glob(f"{root}/**/*.jsonl", recursive=True), key=os.path.getsize, reverse=True
    )
    if nfiles:
        files = files[:nfiles]
    if not files:
        sys.exit(f"no .jsonl under {root}")
    src = sum(os.path.getsize(f) for f in files)
    db = os.path.join(tempfile.mkdtemp(), "bench.db")

    con = open_store(db)
    t0 = time.perf_counter()
    n = sum(sync_file(con, f, root) for f in files)
    el = time.perf_counter() - t0
    con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    idx = os.path.getsize(db)
    con.close()

    print(f"corpus   {len(files)} files, {src / 1e6:.0f} MB")
    print(f"ingest   {n:,} records in {el:.1f}s = {n / el:,.0f} rec/s, {src / 1e6 / el:.0f} MB/s")
    print(f"index    {idx / 1e6:.1f} MB = {100 * idx / src:.1f}% of source")

    ro = connect_ro(db)
    cat = catalog(ro)
    print(f"catalog  {len(cat)} chars, ~{len(cat) // 4} tokens")
    for q in ('"connection refused"', "fsync", "err*"):
        t0 = time.perf_counter()
        search(ro, q, limit=10)
        print(f"search   {q:24} {(time.perf_counter() - t0) * 1000:.2f} ms")
    t0 = time.perf_counter()
    ro.execute("SELECT tool, count(*) FROM ev WHERE tool IS NOT NULL GROUP BY 1").fetchall()
    print(f"groupby  tool over {n:,} rows      {(time.perf_counter() - t0) * 1000:.0f} ms")
    print(f"\ndb kept at {db}")


if __name__ == "__main__":
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    nf = int(a[a.index("--files") + 1]) if "--files" in a else 0
    main(a[0], nf)
