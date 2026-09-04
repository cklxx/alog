use rusqlite::{params, Connection, OpenFlags, Transaction};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::extract::{rec_hash, scan_buf, strip_py, Row};

pub const DDL: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS run (
    sid    INTEGER PRIMARY KEY,
    ext    TEXT NOT NULL UNIQUE,
    path   TEXT NOT NULL UNIQUE,
    size   INTEGER NOT NULL DEFAULT 0,
    cursor INTEGER NOT NULL DEFAULT 0,
    n_ev   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS ev (
    sid     INTEGER NOT NULL,
    seq     INTEGER NOT NULL,
    off     INTEGER NOT NULL,
    len     INTEGER NOT NULL,
    ts      INTEGER,
    kind    TEXT,
    role    TEXT,
    model   TEXT,
    tool    TEXT,
    target  TEXT,
    in_tok  INTEGER,
    out_tok INTEGER,
    cache_r INTEGER,
    is_err  INTEGER,
    crc     INTEGER NOT NULL,
    PRIMARY KEY (sid, seq)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS reject (
    sid INTEGER NOT NULL,
    off INTEGER NOT NULL,
    err TEXT NOT NULL,
    PRIMARY KEY (sid, off)
) WITHOUT ROWID;

-- contentless: 46.3% of text size vs 172% for ordinary fts5.
-- contentless_delete=1 costs +1.7% size and 0.9->3.2ms per query, and is
-- required: a plain contentless table cannot DELETE, so a rewritten source
-- file would leave stale hits behind.
CREATE VIRTUAL TABLE IF NOT EXISTS ftx USING fts5(body, content='', contentless_delete=1);

CREATE TABLE IF NOT EXISTS ftx_map (
    rid INTEGER PRIMARY KEY,
    sid INTEGER NOT NULL,
    seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_ftx_map ON ftx_map(sid, seq);
";

/// Built on demand, after ingest. Measured over the full corpus: ix_err,
/// ix_ts and ix_target together add 134.5 MB, 17.6% of a 763 MB index.
pub const INDEXES: &[(&str, &str)] = &[
    ("kind", "CREATE INDEX IF NOT EXISTS ix_kind ON ev(kind, ts)"),
    ("tool", "CREATE INDEX IF NOT EXISTS ix_tool ON ev(tool, ts)"),
    ("target", "CREATE INDEX IF NOT EXISTS ix_target ON ev(target)"),
    ("ts", "CREATE INDEX IF NOT EXISTS ix_ts ON ev(ts)"),
    (
        "err",
        "CREATE INDEX IF NOT EXISTS ix_err ON ev(is_err) WHERE is_err = 1",
    ),
];

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let con = Connection::open(path)?;
    con.busy_timeout(std::time::Duration::from_secs(30))?;
    con.execute_batch(DDL)?;
    Ok(con)
}

/// `PRAGMA query_only=ON` is not enough on its own: under it ATTACH still
/// succeeds and creates the file. An authorizer is the only complete answer.
/// Note `query_only` also rejects `VACUUM INTO`, so `snapshot` uses `open`.
pub fn open_ro(path: &Path) -> rusqlite::Result<Connection> {
    let con = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    con.pragma_update(None, "query_only", "ON")?;
    con.authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| {
        use rusqlite::hooks::{AuthAction, Authorization};
        match ctx.action {
            AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }));
    Ok(con)
}

/// Path-derived, never the basename: 521 of 10,470 corpus files are
/// named journal.jsonl.
pub fn ext_id(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let s = rel.to_string_lossy();
    s.strip_suffix(".jsonl").unwrap_or(&s).to_string()
}

pub struct Scanned {
    pub path: String,
    pub ext: String,
    pub rows: Vec<Row>,
    pub rejects: Vec<(i64, i64, &'static str)>,
    pub end: i64,
    pub size: i64,
    pub sid: Option<i64>,
    pub base_seq: i64,
    pub fresh: bool,
}

/// Read and parse one file from `start`. No database handle: this is the part
/// that runs in parallel, and SQLite admits one writer.
pub fn scan_file(path: &Path, root: &Path, start: i64, sid: Option<i64>, base_seq: i64)
    -> std::io::Result<Scanned>
{
    let size = std::fs::metadata(path)?.len() as i64;
    let fresh = size < start;
    let from = if fresh { 0 } else { start };
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(from as u64))?;
    let mut buf = Vec::with_capacity((size - from).max(0) as usize + 64);
    f.read_to_end(&mut buf)?;
    let out = scan_buf(&buf, from);
    Ok(Scanned {
        path: path.to_string_lossy().into_owned(),
        ext: ext_id(path, root),
        rows: out.rows,
        rejects: out.rejects,
        end: out.end,
        size,
        sid,
        base_seq: if fresh { 0 } else { base_seq },
        fresh,
    })
}

/// Apply one scanned file inside a caller-owned transaction. Per-file
/// transactions cost 3.7 ms each, which over 10,470 files is 39 s.
pub fn apply(tx: &Transaction, s: &Scanned) -> rusqlite::Result<usize> {
    if s.rows.is_empty() && s.rejects.is_empty() {
        return Ok(0);
    }
    let sid = match s.sid {
        Some(id) => {
            if s.fresh {
                tx.execute("DELETE FROM ev WHERE sid=?1", params![id])?;
                tx.execute("DELETE FROM reject WHERE sid=?1", params![id])?;
                tx.execute(
                    "DELETE FROM ftx WHERE rowid IN (SELECT rid FROM ftx_map WHERE sid=?1)",
                    params![id],
                )?;
                tx.execute("DELETE FROM ftx_map WHERE sid=?1", params![id])?;
            }
            id
        }
        None => {
            tx.execute(
                "INSERT INTO run (ext, path) VALUES (?1, ?2)",
                params![s.ext, s.path],
            )?;
            tx.last_insert_rowid()
        }
    };

    {
        let mut st = tx.prepare_cached(
            "INSERT OR REPLACE INTO ev VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        )?;
        for (i, r) in s.rows.iter().enumerate() {
            st.execute(params![
                sid,
                s.base_seq + i as i64,
                r.off,
                r.len,
                r.ts,
                r.kind,
                r.role,
                r.model,
                r.tool,
                r.target,
                r.in_tok,
                r.out_tok,
                r.cache_r,
                r.is_err,
                r.hash
            ])?;
        }
    }
    if !s.rejects.is_empty() {
        let mut st =
            tx.prepare_cached("INSERT OR REPLACE INTO reject VALUES (?1,?2,?3)")?;
        for (off, _len, err) in &s.rejects {
            st.execute(params![sid, off, err])?;
        }
    }
    {
        let mut next: i64 = tx.query_row(
            "SELECT coalesce(max(rid), 0) + 1 FROM ftx_map",
            [],
            |r| r.get(0),
        )?;
        let mut ft = tx.prepare_cached("INSERT INTO ftx(rowid, body) VALUES (?1, ?2)")?;
        let mut mp = tx.prepare_cached("INSERT INTO ftx_map VALUES (?1, ?2, ?3)")?;
        for (i, r) in s.rows.iter().enumerate() {
            if r.text.is_empty() {
                continue;
            }
            ft.execute(params![next, r.text])?;
            mp.execute(params![next, sid, s.base_seq + i as i64])?;
            next += 1;
        }
    }
    tx.execute(
        "UPDATE run SET size=?1, cursor=?2, n_ev=?3 WHERE sid=?4",
        params![s.size, s.end, s.base_seq + s.rows.len() as i64, sid],
    )?;
    Ok(s.rows.len())
}

/// Read the original bytes of a record. A crc mismatch is an error, never a
/// silently returned wrong record.
pub fn read_record(con: &Connection, sid: i64, seq: i64) -> Result<Vec<u8>, String> {
    let (path, off, len, crc): (String, i64, i64, i64) = con
        .query_row(
            "SELECT r.path, e.off, e.len, e.crc FROM ev e JOIN run r USING (sid)
             WHERE e.sid=?1 AND e.seq=?2",
            params![sid, seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| e.to_string())?;
    let mut f = File::open(&path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(off as u64)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf).map_err(|e| e.to_string())?;
    if rec_hash(strip_py(&buf)) != crc {
        return Err(format!("{path}:{off} changed on disk; run `alog sync`"));
    }
    Ok(buf)
}

pub fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(_) if name.ends_with(".jsonl") || name.ends_with(".ndjson") => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::rec_hash;
    use std::io::Write;

    /// Per-test, not per-process: tests run on threads, and a shared directory
    /// that each one wipes on entry makes every result a race.
    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("alog-t{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(i: usize, text: &str, tool: Option<&str>, err: Option<bool>) -> String {
        let content = match (tool, err) {
            (Some(t), _) => format!(
                r#"[{{"type":"tool_use","name":"{t}","input":{{"file_path":"/a/b.py"}}}}]"#
            ),
            (_, Some(e)) => format!(r#"[{{"type":"tool_result","content":{text:?},"is_error":{e}}}]"#),
            _ => format!(r#"[{{"type":"text","text":{text:?}}}]"#),
        };
        format!(
            r#"{{"uuid":"u{i}","type":"assistant","timestamp":"2026-09-04T12:00:00.000Z","message":{{"role":"assistant","model":"m1","content":{content},"usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#
        )
    }

    fn write_file(d: &Path, name: &str, lines: &[String]) -> std::path::PathBuf {
        let p = d.join(name);
        let mut f = File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        p
    }

    #[test]
    fn roundtrip_tail_and_crc() {
        let d = tmp("roundtrip");
        let p = write_file(
            &d,
            "s.jsonl",
            &[
                rec(0, "hello world", None, None),
                rec(1, "", Some("Bash"), None),
                rec(2, "boom", None, Some(true)),
            ],
        );
        let mut con = open(&d.join("i.db")).unwrap();

        let s = scan_file(&p, &d, 0, None, 0).unwrap();
        assert_eq!(s.rows.len(), 3);
        assert_eq!(s.rows[1].tool.as_deref(), Some("Bash"));
        assert_eq!(s.rows[1].target.as_deref(), Some("/a/b.py"));
        assert_eq!(s.rows[2].is_err, Some(1));
        {
            let tx = con.transaction().unwrap();
            assert_eq!(apply(&tx, &s).unwrap(), 3);
            tx.commit().unwrap();
        }

        // A partial trailing line must not be indexed.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, r#"{{"uuid":"u9","type":"assis"#).unwrap();
        let s2 = scan_file(&p, &d, s.end, Some(1), 3).unwrap();
        assert_eq!(s2.rows.len(), 0, "partial line must be deferred");

        // read_record verifies the original bytes.
        let raw = read_record(&con, 1, 0).unwrap();
        assert!(String::from_utf8_lossy(&raw).contains("\"u0\""));

        // Editing a record in place must raise, never return the wrong bytes.
        let body = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, body.replace("hello world", "HELLO WORLD")).unwrap();
        assert!(read_record(&con, 1, 0).is_err(), "crc must catch a mutation");
    }

    #[test]
    fn rewritten_shorter_file_reindexes() {
        let d = tmp("rewritten");
        let p = write_file(
            &d,
            "t.jsonl",
            &(0..5).map(|i| rec(i, "x", None, None)).collect::<Vec<_>>(),
        );
        let mut con = open(&d.join("t.db")).unwrap();
        let s = scan_file(&p, &d, 0, None, 0).unwrap();
        {
            let tx = con.transaction().unwrap();
            apply(&tx, &s).unwrap();
            tx.commit().unwrap();
        }
        write_file(&d, "t.jsonl", &[rec(0, "x", None, None)]);
        let s2 = scan_file(&p, &d, s.end, Some(1), 5).unwrap();
        assert!(s2.fresh, "a shrunk file must be rescanned from 0");
        assert_eq!(s2.rows.len(), 1);
        {
            let tx = con.transaction().unwrap();
            apply(&tx, &s2).unwrap();
            tx.commit().unwrap();
        }
        let n: i64 = con
            .query_row("SELECT n_ev FROM run", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "stale rows must be gone");
    }

    #[test]
    fn attach_is_denied_and_creates_no_file() {
        let d = tmp("attach");
        let db = d.join("ro.db");
        open(&db).unwrap();
        let con = open_ro(&db).unwrap();
        let evil = d.join("evil.db");
        let sql = format!("ATTACH DATABASE '{}' AS e", evil.display());
        assert!(con.execute_batch(&sql).is_err());
        assert!(!evil.exists(), "ATTACH must not create a file");
        // Writes are refused by the open flag, so dropping query_only is safe.
        assert!(con.execute_batch("DELETE FROM ev").is_err());
        assert!(con.execute_batch("DROP TABLE ev").is_err());
        // Reads still work.
        let _: i64 = con
            .query_row("SELECT count(*) FROM ev", [], |r| r.get(0))
            .unwrap();
    }

    #[test]
    fn hash_includes_length() {
        // crc32 alone collides on same-length edits; the length guard is why
        // rec_hash is (crc32 | len << 32) and why both sides must use it.
        assert_ne!(rec_hash(b"abc"), rec_hash(b"abcd"));
        assert_eq!(rec_hash(b"abc"), rec_hash(b"abc"));
    }

    #[test]
    fn snapshot_works_and_carries_the_rows() {
        // Measured on SQLite 3.51.2: VACUUM INTO fails under query_only=ON, and
        // a read-only handle cannot create the -shm a WAL database needs. So
        // snapshot uses a read-write handle, and open_ro still refuses both.
        let d = tmp("snapshot");
        let db = d.join("s.db");
        let p = write_file(&d, "s.jsonl", &[rec(0, "keep me", None, None)]);
        let mut con = open(&db).unwrap();
        let s = scan_file(&p, &d, 0, None, 0).unwrap();
        let tx = con.transaction().unwrap();
        apply(&tx, &s).unwrap();
        tx.commit().unwrap();

        let out = d.join("snap.db");
        con.execute("VACUUM INTO ?1", params![out.to_str().unwrap()])
            .unwrap();
        let n: i64 = open_ro(&out)
            .unwrap()
            .query_row("SELECT count(*) FROM ev", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(open_ro(&db)
            .unwrap()
            .execute("VACUUM INTO ?1", params![d.join("x.db").to_str().unwrap()])
            .is_err());
    }

    #[test]
    fn ext_id_is_path_derived() {
        let root = Path::new("/r");
        assert_eq!(ext_id(Path::new("/r/a/journal.jsonl"), root), "a/journal");
        assert_eq!(ext_id(Path::new("/r/b/journal.jsonl"), root), "b/journal");
    }
}
