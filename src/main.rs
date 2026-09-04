mod extract;
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
  alog sync <dir>...            index .jsonl trajectory files (repeatable, incremental)
  alog view [<dir>]             scan if needed, then open the browser viewer
  alog search <query>           full-text search; fts5 syntax
  alog show <session> [seq]     one session's timeline, or one record
  alog sql <query>              read-only SQL over the index
  alog catalog                  what is in the store, schema and distributions
  alog errors                   failed tool calls, newest first
  alog dump [<session>]         byte-identical jsonl to stdout
  alog index <col>              build a secondary index: kind tool target ts err
  alog snapshot <out.db>        consistent copy (VACUUM INTO, never cp)
  alog stats                    ingest and size numbers

OPTIONS
  --db <path>     index location (default ~/.alog/index.db, or $ALOG_DB)
  --limit <n>     max rows (default 50; search 20)
  --port <n>      viewer port (default 8877)
  --no-snippets   skip source reads in search results
  -j <n>          scan threads (default: cores)
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("alog {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(e) = run(&args) {
        eprintln!("alog: {e}");
        std::process::exit(1);
    }
}

struct Opts {
    db: PathBuf,
    limit: usize,
    port: u16,
    snippets: bool,
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
            if o.rest.is_empty() {
                return Err("sync needs a directory".into());
            }
            for d in &o.rest {
                let (files, records, bare, el) = sync(&o.db, Path::new(d), o.threads)?;
                println!(
                    "{records} records from {files} files in {:.1}s ({:.0} rec/s)",
                    el,
                    records as f64 / el.max(0.001)
                );
                // Silence here would be the defect: a foreign format parses as
                // valid JSON, is stored, is dumpable -- and has every semantic
                // column NULL and no full-text entry. Measured on the real
                // corpus, 45.9% of records legitimately carry no role or tool
                // (bookkeeping), so the signal is a total absence of extracted
                // text, not a high share of bare rows.
                if records > 0 && bare == records {
                    eprintln!(
                        "alog: none of the {records} records yielded a role, tool or searchable \
                         text. The extractor targets Claude Code session files; another format \
                         is indexed and dumpable, but not searchable."
                    );
                }
            }
        }
        "view" => {
            if let Some(d) = o.rest.first() {
                let (_, records, _, el) = sync(&o.db, Path::new(d), o.threads)?;
                if records > 0 {
                    println!("indexed {records} records in {el:.1}s");
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
            println!("{}", query::search(&con, &q, n, o.snippets));
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
                    print!("{}", String::from_utf8_lossy(&raw));
                }
                None => println!(
                    "{}",
                    query::outline(&con, ext, 0, if o.limit == 0 { 80 } else { o.limit })
                ),
            }
        }
        "sql" => {
            let con = ro(&o.db)?;
            let q = o.rest.join(" ");
            println!(
                "{}",
                query::query(&con, &q, if o.limit == 0 { 50 } else { o.limit })
            );
        }
        "catalog" => {
            let con = ro(&o.db)?;
            print!("{}", query::catalog(&con).map_err(|e| e.to_string())?);
        }
        "errors" => {
            let con = ro(&o.db)?;
            let n = if o.limit == 0 { 50 } else { o.limit };
            println!(
                "{}",
                query::query(
                    &con,
                    &format!(
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
                    ),
                    n
                )
            );
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
            println!("built ix_{name} in {:.1}s", t.elapsed().as_secs_f64());
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
            println!("{out}");
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
            println!("events    {ev}");
            println!("sessions  {runs}");
            println!("rejects   {rej}");
            println!("indexed   {:.2} GB", bytes as f64 / 1e9);
            println!(
                "index     {:.0} MB = {:.1}% of source",
                idx as f64 / 1e6,
                100.0 * idx as f64 / bytes.max(1) as f64
            );
        }
        other => return Err(format!("unknown command {other}\n\n{USAGE}")),
    }
    Ok(())
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
/// the part that scales. Measured 2138 MB/s at 14 threads vs 274 MB/s for a
/// single-threaded Python scan of the same corpus.
fn sync(db: &Path, root: &Path, threads: usize) -> Result<(usize, usize, usize, f64), String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let mut con = store::open(db).map_err(|e| e.to_string())?;
    let paths = store::walk(&root);
    if paths.is_empty() {
        return Err(format!("no .jsonl files under {}", root.display()));
    }

    // Existing cursors, so an unchanged file is skipped without opening it.
    let mut known = std::collections::HashMap::new();
    {
        let mut st = con
            .prepare("SELECT path, sid, cursor, n_ev FROM run")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?),
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows.flatten() {
            known.insert(row.0, row.1);
        }
    }

    let todo: Vec<(PathBuf, Option<i64>, i64, i64)> = paths
        .into_iter()
        .filter_map(|p| {
            let size = std::fs::metadata(&p).ok()?.len() as i64;
            match known.get(&p.to_string_lossy().to_string()) {
                Some(&(_, cursor, _)) if cursor == size => None,
                Some(&(sid, cursor, n_ev)) => Some((p, Some(sid), cursor, n_ev)),
                None => Some((p, None, 0, 0)),
            }
        })
        .collect();
    if todo.is_empty() {
        return Ok((0, 0, 0, 0.0));
    }

    let t0 = Instant::now();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::sync_channel::<store::Scanned>(threads * 4);
    let rootc = root.clone();
    pool.spawn(move || {
        todo.into_par_iter()
            .for_each_with(tx, |tx, (p, sid, cursor, base)| {
                if let Ok(s) = store::scan_file(&p, &rootc, cursor, sid, base) {
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
                    .filter(|r| {
                        r.role.is_none() && r.tool.is_none() && r.text.is_empty()
                    })
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
    Ok((files, records, bare, t0.elapsed().as_secs_f64()))
}

fn dump(con: &rusqlite::Connection, session: Option<&str>) -> Result<(), String> {
    use std::io::Write;
    let sql = match session {
        Some(_) => "SELECT e.sid, e.seq FROM ev e JOIN run r USING (sid)
                    WHERE r.ext = ?1 ORDER BY e.seq",
        None => "SELECT e.sid, e.seq FROM ev e JOIN run r USING (sid)
                 ORDER BY r.ext, e.seq",
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
    for (sid, seq) in rows {
        match store::read_record(con, sid, seq) {
            Ok(raw) => {
                w.write_all(extract::strip_py(&raw)).map_err(|e| e.to_string())?;
                w.write_all(b"\n").map_err(|e| e.to_string())?;
            }
            Err(e) => eprintln!("alog: skipped {sid}/{seq}: {e}"),
        }
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
