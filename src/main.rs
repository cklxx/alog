mod extract;
mod mcp;
mod query;
mod serve;
mod store;

use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

const USAGE: &str = "\
alog -- agent trajectories, queryable

USAGE
  alog sync [<dir>...]          index sessions; no arg finds them under ~
  alog view [<dir>...]          sync, then open the browser viewer
  alog search <query>           full-text search; fts5 syntax
  alog show <session> [seq]     one session's timeline, or one record
  alog turns <session>          one line per user request: work, cost, errors
  alog sql <query>              read-only SQL over the index
  alog catalog                  what is in the store, schema and distributions
  alog errors                   failed tool calls, newest first
  alog dump [<session>]         byte-identical jsonl to stdout
  alog index <col>              build a secondary index: kind tool target ts err
  alog snapshot <out.db>        consistent copy (VACUUM INTO, never cp)
  alog doctor [all]             verify the index against the files
  alog mcp                      MCP server over stdio, for an agent
  alog stats                    ingest and size numbers

OPTIONS
  --db <path>     index location (default ~/.alog/index.db, or $ALOG_DB)
  --limit <n>     max rows (default 50; search 20)
  --port <n>      viewer port (default 8877)
  --json          machine-readable output, and errors as {error:{kind,hint,...}}
  --no-snippets   skip source reads in search results
  -j <n>          scan threads (default: cores)
";

/// stdout that dies quietly when the reader goes away. Rust ignores SIGPIPE so
/// an EPIPE surfaces as an error, and `println!` turns that into a panic -- so
/// `alog search ... | head` printed a panic instead of exiting. Every command
/// writes through these.
macro_rules! pln {
    ($($a:tt)*) => {{
        use std::io::Write;
        if writeln!(std::io::stdout(), $($a)*).is_err() {
            std::process::exit(0);
        }
    }};
}

macro_rules! pr {
    ($($a:tt)*) => {{
        use std::io::Write;
        if write!(std::io::stdout(), $($a)*).is_err() {
            std::process::exit(0);
        }
    }};
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        pr!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        pln!("alog {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let json = args.iter().any(|a| a == "--json");
    if let Err(e) = run(&args) {
        if json {
            // Same envelope the MCP server returns, so an agent branches on
            // `kind` either way. stderr, so it never mixes with result data.
            eprintln!("{}", fail(&e));
        } else {
            eprintln!("alog: {e}");
        }
        std::process::exit(1);
    }
}

/// Classify a top-level failure for a machine reader: `kind` is the stable
/// string to branch on, `hint` says what to do, `retryable` says whether doing
/// the same thing again could work. Errors that already carry an ALOG_ code
/// from query.rs pass through untouched.
fn fail(e: &str) -> String {
    let (kind, hint, retryable) = if e.starts_with("no index at") {
        ("no-index", "run `alog sync` to build it", false)
    } else if e.contains("no session directory found") {
        ("no-session-directory", "pass a directory explicitly", false)
    } else if e.starts_with("no session") {
        (
            "unknown-session",
            "list sessions with `alog sql \"SELECT ext FROM run\"`",
            false,
        )
    } else if e.contains("no .jsonl files under") {
        (
            "empty-directory",
            "point at a directory containing .jsonl sessions",
            false,
        )
    } else if e.contains("No such file or directory") {
        ("no-such-path", "check the path exists", false)
    } else if e.contains("records unreadable") {
        (
            "stale-index",
            "the source changed on disk; run `alog sync`",
            true,
        )
    } else if e.starts_with("unknown command") || e.starts_with("no command") {
        ("bad-usage", "see `alog --help`", false)
    } else if e.ends_with("exists") {
        ("output-exists", "choose a path that does not exist", false)
    } else if e.contains("database is locked") || e.contains("busy") {
        ("locked", "another writer holds the index; retry", true)
    } else {
        ("error", "", false)
    };
    let first = e.lines().next().unwrap_or(e);
    format!(
        "{{\"error\":{{\"kind\":{:?},\"message\":{:?},\"hint\":{:?},\"retryable\":{}}}}}",
        kind, first, hint, retryable
    )
}

struct Opts {
    db: PathBuf,
    limit: usize,
    port: u16,
    snippets: bool,
    json: bool,
    threads: usize,
    rest: Vec<String>,
}

/// Options may appear before the command, so the command is the first argument
/// that is neither a flag nor a flag's value.
fn parse(args: &[String]) -> Opts {
    let mut o = Opts {
        db: std::env::var("ALOG_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let mut p = dirs_home();
                p.push(".alog");
                p.push("index.db");
                p
            }),
        limit: 0,
        port: 8877,
        snippets: true,
        json: false,
        threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
        rest: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut val = || {
            i += 1;
            args.get(i).cloned().unwrap_or_default()
        };
        match a {
            "--db" => o.db = PathBuf::from(val()),
            "--limit" | "-n" => o.limit = val().parse().unwrap_or(50),
            "--port" => o.port = val().parse().unwrap_or(8877),
            "-j" => o.threads = val().parse().unwrap_or(o.threads).max(1),
            "--no-snippets" => o.snippets = false,
            "--json" => o.json = true,
            _ => o.rest.push(a.to_string()),
        }
        i += 1;
    }
    o
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Where agent frameworks keep sessions. `sync` and `view` with no argument
/// index every one that exists, so the common case takes no path at all.
fn default_roots() -> Vec<PathBuf> {
    let home = dirs_home();
    ["/.claude/projects", "/.codex/sessions", "/.dsh/sessions"]
        .iter()
        .map(|p| {
            let mut d = home.clone();
            d.push(p.trim_start_matches('/'));
            d
        })
        .filter(|d| d.is_dir())
        .collect()
}

/// The directories to index: what was asked for, else every framework's own.
fn roots(given: &[String]) -> Result<Vec<PathBuf>, String> {
    if !given.is_empty() {
        return Ok(given.iter().map(PathBuf::from).collect());
    }
    let found = default_roots();
    if found.is_empty() {
        return Err("no session directory found under ~; pass one".into());
    }
    Ok(found)
}

fn run(args: &[String]) -> Result<(), String> {
    let mut o = parse(args);
    if o.rest.is_empty() {
        return Err(format!("no command\n\n{USAGE}"));
    }
    let cmd = o.rest.remove(0);
    let cmd = cmd.as_str();
    if let Some(p) = o.db.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }

    match cmd {
        "sync" => {
            for d in roots(&o.rest)? {
                let (files, records, bare, gone, el) = sync(&o.db, &d, o.threads)?;
                pln!(
                    "{records} records from {files} files in {:.1}s ({:.0} rec/s)  {}",
                    el,
                    records as f64 / el.max(0.001),
                    d.display()
                );
                if gone > 0 {
                    pln!("dropped {gone} sessions whose files are gone");
                }
                // An unknown format parses as valid JSON, stores, and dumps
                // -- with every semantic column NULL and nothing searchable.
                // The signal is a total absence of extracted text: 45.9% of
                // the real corpus legitimately carries no role or tool.
                if records > 0 && bare == records {
                    eprintln!(
                        "alog: none of the {records} records yielded a role, tool or searchable \
                         text. Claude Code and Codex CLI sessions are understood; this format \
                         is indexed and dumpable, but not searchable."
                    );
                }
            }
        }
        "view" => {
            for d in roots(&o.rest)? {
                if let Ok((_, records, _, _, el)) = sync(&o.db, &d, o.threads) {
                    if records > 0 {
                        pln!("indexed {records} records in {el:.1}s  {}", d.display());
                    }
                }
            }
            serve::run(&o.db, o.port)?;
        }
        "search" => {
            let con = ro(&o.db)?;
            let q = o.rest.join(" ");
            if q.is_empty() {
                return Err("search needs a query".into());
            }
            let n = if o.limit == 0 { 20 } else { o.limit };
            pln!("{}", query::search(&con, &q, n, o.snippets));
        }
        "show" => {
            let con = ro(&o.db)?;
            let ext = o.rest.first().ok_or("show needs a session")?;
            match o.rest.get(1) {
                Some(seq) => {
                    let sid: i64 = con
                        .query_row(
                            "SELECT sid FROM run WHERE ext=?1",
                            rusqlite::params![ext],
                            |r| r.get(0),
                        )
                        .map_err(|_| format!("no session {ext}"))?;
                    let seq: i64 = seq.parse().map_err(|_| "seq must be a number")?;
                    let raw = store::read_record(&con, sid, seq)?;
                    pr!("{}", String::from_utf8_lossy(&raw));
                }
                None => pln!(
                    "{}",
                    query::outline(&con, ext, 0, if o.limit == 0 { 80 } else { o.limit })
                ),
            }
        }
        "turns" => {
            let con = ro(&o.db)?;
            let ext = o.rest.first().ok_or("turns needs a session")?;
            pln!(
                "{}",
                query::turns(&con, ext, if o.limit == 0 { 60 } else { o.limit })
            );
        }
        "sql" => {
            let con = ro(&o.db)?;
            let q = o.rest.join(" ");
            pln!(
                "{}",
                query::query(&con, &q, if o.limit == 0 { 50 } else { o.limit })
            );
        }
        "catalog" => {
            let con = ro(&o.db)?;
            pr!("{}", query::catalog(&con).map_err(|e| e.to_string())?);
        }
        "errors" => {
            let con = ro(&o.db)?;
            let n = if o.limit == 0 { 50 } else { o.limit };
            pln!("{}", query::query(&con, &errors_sql(n), n));
        }
        "dump" => {
            let con = ro(&o.db)?;
            dump(&con, o.rest.first().map(|s| s.as_str()))?;
        }
        "index" => {
            let name = o.rest.first().ok_or("index needs a column name")?;
            let sql = store::INDEXES
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    format!(
                        "unknown index {name}; known: {}",
                        store::INDEXES
                            .iter()
                            .map(|(k, _)| *k)
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                })?;
            let con = store::open(&o.db).map_err(|e| e.to_string())?;
            let t = Instant::now();
            con.execute_batch(sql).map_err(|e| e.to_string())?;
            pln!("built ix_{name} in {:.1}s", t.elapsed().as_secs_f64());
        }
        "snapshot" => {
            let out = o.rest.first().ok_or("snapshot needs an output path")?;
            if Path::new(out).exists() {
                return Err(format!("{out} exists"));
            }
            // A read-write handle, deliberately: VACUUM INTO fails under
            // query_only=ON, and a read-only handle cannot create the -shm file
            // a WAL database needs. Never `cp` -- that loses the WAL silently
            // while integrity_check still returns ok.
            let con = store::open(exists(&o.db)?).map_err(|e| e.to_string())?;
            con.execute("VACUUM INTO ?1", rusqlite::params![out])
                .map_err(|e| e.to_string())?;
            pln!("{out}");
        }
        "mcp" => {
            mcp::run(exists(&o.db)?)?;
        }
        "doctor" => {
            let con = ro(&o.db)?;
            // One record per session by default. Re-hashing 3 each over 10,470
            // sessions took 37 s, which is too slow to run casually; 1 each is
            // 13 s and still catches a rewritten file, because a rewrite moves
            // every offset after the edit. `--limit 0` checks every record.
            let n = if o.limit == 0 { 1 } else { o.limit };
            let n = if o.rest.iter().any(|a| a == "all") {
                0
            } else {
                n
            };
            let (ok, report) = query::doctor(&con, n, o.json);
            pln!("{report}");
            if !ok {
                std::process::exit(1);
            }
        }
        "stats" => {
            let con = ro(&o.db)?;
            let (ev, runs, rej, bytes): (i64, i64, i64, i64) = con
                .query_row(
                    "SELECT (SELECT count(*) FROM ev), (SELECT count(*) FROM run),
                            (SELECT count(*) FROM reject),
                            (SELECT coalesce(sum(size),0) FROM run)",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .map_err(|e| e.to_string())?;
            let idx = std::fs::metadata(&o.db).map(|m| m.len()).unwrap_or(0);
            pln!("events    {ev}");
            pln!("sessions  {runs}");
            pln!("rejects   {rej}");
            pln!("indexed   {:.2} GB", bytes as f64 / 1e9);
            pln!(
                "index     {:.0} MB = {:.1}% of source",
                idx as f64 / 1e6,
                100.0 * idx as f64 / bytes.max(1) as f64
            );
        }
        other => return Err(format!("unknown command {other}\n\n{USAGE}")),
    }
    Ok(())
}

/// Failed tool calls, newest first. A failure record often carries no tool name
/// of its own, so it inherits the one from the call it is answering.
pub fn errors_sql(n: usize) -> String {
    format!(
        "SELECT r.ext AS session, e.seq,
                datetime(e.ts/1000, 'unixepoch', 'localtime') AS at,
                coalesce(e.tool, (SELECT p.tool FROM ev p
                    WHERE p.sid = e.sid AND p.seq < e.seq
                    AND p.tool IS NOT NULL
                    ORDER BY p.seq DESC LIMIT 1)) AS tool,
                coalesce(e.target, (SELECT p.target FROM ev p
                    WHERE p.sid = e.sid AND p.seq < e.seq
                    AND p.tool IS NOT NULL
                    ORDER BY p.seq DESC LIMIT 1)) AS target
         FROM ev e JOIN run r USING (sid) WHERE e.is_err = 1
         ORDER BY e.ts DESC LIMIT {n}"
    )
}

fn exists(db: &Path) -> Result<&Path, String> {
    if db.exists() {
        Ok(db)
    } else {
        Err(format!(
            "no index at {}; run `alog sync <dir>` first",
            db.display()
        ))
    }
}

fn ro(db: &Path) -> Result<rusqlite::Connection, String> {
    store::open_ro(exists(db)?).map_err(|e| e.to_string())
}

/// Scan in parallel, write serially: SQLite admits one writer, and the scan is
/// the part that scales. Measured 2138 MB/s at 14 threads.
///
/// Returns (files, records, bare, gone, elapsed).
fn sync(
    db: &Path,
    root: &Path,
    threads: usize,
) -> Result<(usize, usize, usize, usize, f64), String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let mut con = store::open(db).map_err(|e| e.to_string())?;
    let paths = store::walk(&root);
    if paths.is_empty() {
        return Err(format!("no .jsonl files under {}", root.display()));
    }
    let t0 = Instant::now();

    // What the index already holds, and the tail record that proves a cursor is
    // still valid. Reading it here costs one query; trusting a byte length
    // instead cost 100% of a rewritten file's rows.
    let mut known: std::collections::HashMap<String, store::Prev> = Default::default();
    {
        let mut st = con
            .prepare(
                "SELECT r.path, r.sid, r.cursor, r.n_ev, e.off, e.len, e.crc
                 FROM run r LEFT JOIN ev e
                   ON e.sid = r.sid AND e.seq = r.n_ev - 1",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                let tail = match (r.get(4)?, r.get(5)?, r.get(6)?) {
                    (Some(o), Some(l), Some(c)) => Some((o, l, c)),
                    _ => None,
                };
                Ok((
                    r.get::<_, String>(0)?,
                    store::Prev {
                        sid: r.get(1)?,
                        cursor: r.get(2)?,
                        n_ev: r.get(3)?,
                        tail,
                    },
                ))
            })
            .map_err(|e| e.to_string())?;
        for (path, prev) in rows.flatten() {
            known.insert(path, prev);
        }
    }

    // A source file that is gone must take its rows with it. Only paths under
    // this root are considered, so syncing one directory never touches another.
    let live: std::collections::HashSet<&String> = Default::default();
    let mut live = live;
    let keys: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into()).collect();
    for k in &keys {
        live.insert(k);
    }
    let mut gone = 0usize;
    {
        let stale: Vec<i64> = known
            .iter()
            .filter(|(path, _)| !live.contains(path) && Path::new(path).starts_with(&root))
            .map(|(_, p)| p.sid)
            .collect();
        if !stale.is_empty() {
            let tx = con.transaction().map_err(|e| e.to_string())?;
            for sid in &stale {
                store::forget(&tx, *sid).map_err(|e| e.to_string())?;
                tx.execute("DELETE FROM run WHERE sid=?1", rusqlite::params![sid])
                    .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())?;
            gone = stale.len();
        }
    }

    // sid is assigned here, in sorted path order, not by whichever thread
    // finished first: it is the ev primary key and appears in every `alog sql`
    // result, and a nondeterministic one made 99.9% of sessions change id
    // between two builds of the same files.
    let mut todo: Vec<(PathBuf, Option<store::Prev>, i64)> = Vec::new();
    {
        let tx = con.transaction().map_err(|e| e.to_string())?;
        for (p, key) in paths.into_iter().zip(keys) {
            let prev = known.remove(&key);
            match prev {
                // Every known file is handed to the scanner, which decides
                // whether to resume by re-hashing the record at the cursor. No
                // metadata shortcut stands in front of that: size and mtime are
                // both restorable, and a file that restores them is skipped.
                Some(pv) => {
                    let sid = pv.sid;
                    todo.push((p, Some(pv), sid))
                }
                None => {
                    tx.execute(
                        "INSERT INTO run (ext, path) VALUES (?1, ?2)",
                        rusqlite::params![store::ext_id(&p, &root), key],
                    )
                    .map_err(|e| e.to_string())?;
                    let sid = tx.last_insert_rowid();
                    todo.push((p, None, sid));
                }
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
    }
    if todo.is_empty() {
        return Ok((0, 0, 0, gone, t0.elapsed().as_secs_f64()));
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::sync_channel::<store::Scanned>(threads * 4);
    pool.spawn(move || {
        todo.into_par_iter()
            .for_each_with(tx, |tx, (p, prev, sid)| {
                if let Ok(s) = store::scan_file(&p, prev.as_ref(), sid) {
                    let _ = tx.send(s);
                }
            });
    });

    // 64 files per transaction: a per-file transaction costs 3.7 ms, which over
    // 10,470 files is 39 s of pure overhead against ~20 us of real work each.
    let mut files = 0usize;
    let mut records = 0usize;
    let mut bare = 0usize;
    let mut batch = Vec::with_capacity(64);
    let flush = |batch: &mut Vec<store::Scanned>,
                 con: &mut rusqlite::Connection,
                 files: &mut usize,
                 records: &mut usize,
                 bare: &mut usize|
     -> Result<(), String> {
        if batch.is_empty() {
            return Ok(());
        }
        let tx = con.transaction().map_err(|e| e.to_string())?;
        for s in batch.iter() {
            let n = store::apply(&tx, s).map_err(|e| format!("{}: {e}", s.path))?;
            if n > 0 {
                *files += 1;
                *records += n;
                *bare += s
                    .rows
                    .iter()
                    .filter(|r| r.role.is_none() && r.tool.is_none() && r.text.is_empty())
                    .count();
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        batch.clear();
        Ok(())
    };
    for s in rx {
        batch.push(s);
        if batch.len() == 64 {
            flush(&mut batch, &mut con, &mut files, &mut records, &mut bare)?;
        }
    }
    flush(&mut batch, &mut con, &mut files, &mut records, &mut bare)?;
    Ok((files, records, bare, gone, t0.elapsed().as_secs_f64()))
}

fn dump(con: &rusqlite::Connection, session: Option<&str>) -> Result<(), String> {
    use std::io::Write;
    let sql = match session {
        Some(_) => {
            "SELECT e.sid, e.seq FROM ev e JOIN run r USING (sid)
                    WHERE r.ext = ?1 ORDER BY e.seq"
        }
        None => {
            "SELECT e.sid, e.seq FROM ev e JOIN run r USING (sid)
                 ORDER BY r.ext, e.seq"
        }
    };
    let mut st = con.prepare(sql).map_err(|e| e.to_string())?;
    let rows: Vec<(i64, i64)> = match session {
        Some(s) => st
            .query_map(rusqlite::params![s], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect(),
        None => st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect(),
    };
    let out = std::io::stdout();
    let mut w = std::io::BufWriter::new(out.lock());
    // A skip is a failure, not a warning. Exiting 0 after writing 0 bytes told
    // a caller the dump succeeded and the session was empty.
    let mut skipped = 0usize;
    let mut first = String::new();
    for (sid, seq) in rows {
        match store::read_record(con, sid, seq) {
            Ok(raw) => {
                if w.write_all(extract::strip_ws(&raw)).is_err() || w.write_all(b"\n").is_err() {
                    return Ok(()); // reader closed the pipe
                }
            }
            Err(e) => {
                if skipped == 0 {
                    first = e;
                }
                skipped += 1;
            }
        }
    }
    if w.flush().is_err() {
        return Ok(());
    }
    if skipped > 0 {
        return Err(format!("{skipped} records unreadable; first: {first}"));
    }
    Ok(())
}

pub fn fmt_ts(ms: i64) -> String {
    // Local-time-free: the index stores epoch ms and every consumer formats it.
    let secs = ms / 1000;
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
