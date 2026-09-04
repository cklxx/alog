use rusqlite::{params, Connection};
use std::path::Path;
use tiny_http::{Header, Request, Response, Server};

use crate::query::{preview, snip, terms_of, KEEP};
use crate::store::{open_ro, read_record};

const VIEWER: &str = include_str!("viewer.html");

pub fn run(db: &Path, port: u16) -> Result<(), String> {
    if !db.exists() {
        return Err(format!("no index at {}; run `alog sync <dir>`", db.display()));
    }
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| e.to_string())?;
    let url = format!("http://{addr}");
    println!("serving {url}  (ctrl-c to stop)");
    open_browser(&url);
    let db = db.to_path_buf();
    for req in server.incoming_requests() {
        let dbc = db.clone();
        std::thread::spawn(move || handle(req, &dbc));
    }
    Ok(())
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

fn handle(req: Request, db: &Path) {
    let url = req.url().to_string();
    let (path, qs) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let q = |k: &str| -> Option<String> {
        qs.split('&').find_map(|kv| {
            let (a, b) = kv.split_once('=')?;
            (a == k).then(|| urldecode(b))
        })
    };
    let n = |k: &str, d: i64| q(k).and_then(|v| v.parse().ok()).unwrap_or(d);

    if path == "/" || path == "/index.html" {
        let h = Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
        let _ = req.respond(Response::from_string(VIEWER).with_header(h));
        return;
    }
    let con = match open_ro(db) {
        Ok(c) => c,
        Err(e) => return send(req, 500, &err_json(&e.to_string())),
    };
    let body = match path {
        "/stats" => stats(&con),
        "/sessions" => sessions(&con, n("limit", 300)),
        "/timeline" => timeline(&con, n("sid", -1), n("start", 0), n("limit", 300)),
        "/record" => record(&con, n("sid", -1), n("seq", -1)),
        "/search" => search(&con, &q("q").unwrap_or_default(), n("limit", 60)),
        "/errors" => errors(&con, n("limit", 120)),
        _ => return send(req, 404, &err_json("not found")),
    };
    match body {
        Ok(s) => send(req, 200, &s),
        Err(e) => send(req, 500, &err_json(&e)),
    }
}

fn send(req: Request, code: u16, body: &str) {
    let h = Header::from_bytes("Content-Type", "application/json").unwrap();
    let _ = req.respond(
        Response::from_string(body)
            .with_status_code(code)
            .with_header(h),
    );
}

fn err_json(m: &str) -> String {
    format!("{{\"error\":{}}}", jstr(m))
}

/// Minimal JSON string escaping. serde_json is a dependency but building a
/// Value per row costs more than writing the bytes directly.
fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                o.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn jopt(v: &Option<String>) -> String {
    v.as_ref().map(|s| jstr(s)).unwrap_or_else(|| "null".into())
}

fn jnum(v: Option<i64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "null".into())
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("20");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b' '));
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn stats(con: &Connection) -> Result<String, String> {
    let (ev, runs, rej, bytes): (i64, i64, i64, i64) = con
        .query_row(
            "SELECT (SELECT count(*) FROM ev), (SELECT count(*) FROM run),
                    (SELECT count(*) FROM reject),
                    (SELECT coalesce(sum(size),0) FROM run)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| e.to_string())?;
    let group = |col: &str, cond: &str, lim: i64| -> Result<String, String> {
        let mut st = con
            .prepare(&format!(
                "SELECT {col}, count(*) FROM ev WHERE {cond}
                 GROUP BY 1 ORDER BY 2 DESC LIMIT {lim}"
            ))
            .map_err(|e| e.to_string())?;
        let rows: Vec<(Option<String>, i64)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        Ok(format!(
            "[{}]",
            rows.iter()
                .map(|(k, v)| format!("[{},{v}]", jopt(k)))
                .collect::<Vec<_>>()
                .join(",")
        ))
    };
    Ok(format!(
        "{{\"events\":{ev},\"sessions\":{runs},\"rejects\":{rej},\"bytes\":{bytes},\
          \"tools\":{},\"errors\":{},\"models\":{}}}",
        group("tool", "tool IS NOT NULL", 12)?,
        group("tool", "is_err = 1", 8)?,
        group("model", "model IS NOT NULL", 8)?
    ))
}

fn sessions(con: &Connection, limit: i64) -> Result<String, String> {
    let mut st = con
        .prepare(
            "SELECT r.ext, r.sid, r.n_ev, min(e.ts), max(e.ts), sum(e.is_err = 1),
                    sum(coalesce(e.in_tok,0) + coalesce(e.out_tok,0))
             FROM run r LEFT JOIN ev e USING (sid)
             GROUP BY r.sid ORDER BY max(e.ts) DESC LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<String> = st
        .query_map(params![limit], |r| {
            Ok(format!(
                "{{\"ext\":{},\"sid\":{},\"n\":{},\"t0\":{},\"t1\":{},\"err\":{},\"tok\":{}}}",
                jstr(&r.get::<_, String>(0)?),
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                jnum(r.get(3)?),
                jnum(r.get(4)?),
                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                r.get::<_, Option<i64>>(6)?.unwrap_or(0),
            ))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    Ok(format!("[{}]", rows.join(",")))
}

/// Each row carries a text preview read from the source file: without it, 37%
/// of rows measured as a bare role with no other field set.
fn timeline(con: &Connection, sid: i64, start: i64, limit: i64) -> Result<String, String> {
    let mut st = con
        .prepare(&format!(
            "SELECT seq, ts, kind, role, model, tool, target, is_err, len,
                    in_tok, out_tok, cache_r FROM ev
             WHERE sid = ?1 AND seq >= ?2 AND {KEEP} ORDER BY seq LIMIT ?3"
        ))
        .map_err(|e| e.to_string())?;
    let raw: Vec<(i64, Option<i64>, Option<String>, Option<String>, Option<String>,
                  Option<String>, Option<String>, Option<i64>, i64,
                  Option<i64>, Option<i64>, Option<i64>)> = st
        .query_map(params![sid, start, limit], |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let (n_ev, shown): (i64, i64) = con
        .query_row(
            &format!(
                "SELECT n_ev, (SELECT count(*) FROM ev WHERE sid = ?1 AND {KEEP})
                 FROM run WHERE sid = ?1"
            ),
            params![sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;

    let rows: Vec<String> = raw
        .iter()
        .map(|(seq, ts, kind, role, model, tool, target, err, len, i, o, c)| {
            // Read the source only when the indexed columns leave the row blank:
            // 37% of rows measured as a bare role, and TaskCreate/Agent/ToolSearch
            // carry their whole payload in an input object with no path-like target.
            let text = if target.is_none() || *err == Some(1) {
                preview(con, sid, *seq, 180)
            } else {
                String::new()
            };
            format!(
                "{{\"seq\":{seq},\"ts\":{},\"kind\":{},\"role\":{},\"model\":{},\"tool\":{},\
                  \"target\":{},\"err\":{},\"len\":{len},\"in\":{},\"out\":{},\"cache\":{},\
                  \"text\":{}}}",
                jnum(*ts), jopt(kind), jopt(role), jopt(model), jopt(tool), jopt(target),
                jnum(*err), jnum(*i), jnum(*o), jnum(*c), jstr(&text)
            )
        })
        .collect();
    Ok(format!(
        "{{\"n_ev\":{n_ev},\"n_shown\":{shown},\"hidden\":{},\"rows\":[{}]}}",
        n_ev - shown,
        rows.join(",")
    ))
}

fn record(con: &Connection, sid: i64, seq: i64) -> Result<String, String> {
    match read_record(con, sid, seq) {
        Ok(raw) => match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(v) => Ok(format!(
                "{{\"json\":{}}}",
                serde_json::to_string(&v).unwrap_or_else(|_| "null".into())
            )),
            Err(_) => Ok(format!(
                "{{\"raw\":{}}}",
                jstr(&String::from_utf8_lossy(&raw))
            )),
        },
        Err(e) => Ok(err_json(&e)),
    }
}

fn search(con: &Connection, term: &str, limit: i64) -> Result<String, String> {
    if term.trim().is_empty() {
        return Ok("{\"hits\":[]}".into());
    }
    let mut st = match con.prepare(
        "SELECT r.ext, m.sid, m.seq, e.ts, e.kind, e.tool, e.target, e.is_err FROM ftx
         JOIN ftx_map m ON m.rid = ftx.rowid
         JOIN ev e ON e.sid = m.sid AND e.seq = m.seq
         JOIN run r ON r.sid = m.sid
         WHERE ftx MATCH ?1 ORDER BY rank LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(e) => return Ok(err_json(&e.to_string())),
    };
    let rows: Vec<(String, i64, i64, Option<i64>, Option<String>, Option<String>,
                   Option<String>, Option<i64>)> =
        match st.query_map(params![term, limit], |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                r.get(7)?,
            ))
        }) {
            Ok(it) => it.flatten().collect(),
            Err(e) => return Ok(err_json(&e.to_string())),
        };
    let words = terms_of(term);
    let hits: Vec<String> = rows
        .iter()
        .map(|(ext, sid, seq, ts, kind, tool, target, err)| {
            let s = read_record(con, *sid, *seq)
                .map(|b| snip(&String::from_utf8_lossy(&b), &words, 200))
                .unwrap_or_default();
            format!(
                "{{\"ext\":{},\"sid\":{sid},\"seq\":{seq},\"ts\":{},\"kind\":{},\"tool\":{},\
                  \"target\":{},\"err\":{},\"snip\":{}}}",
                jstr(ext), jnum(*ts), jopt(kind), jopt(tool), jopt(target), jnum(*err), jstr(&s)
            )
        })
        .collect();
    Ok(format!("{{\"hits\":[{}]}}", hits.join(",")))
}

fn errors(con: &Connection, limit: i64) -> Result<String, String> {
    let mut st = con
        .prepare(
            "SELECT r.ext, e.sid, e.seq, e.ts,
                    coalesce(e.tool, (SELECT p.tool FROM ev p WHERE p.sid = e.sid
                        AND p.seq < e.seq AND p.tool IS NOT NULL
                        ORDER BY p.seq DESC LIMIT 1)),
                    coalesce(e.target, (SELECT p.target FROM ev p WHERE p.sid = e.sid
                        AND p.seq < e.seq AND p.tool IS NOT NULL
                        ORDER BY p.seq DESC LIMIT 1))
             FROM ev e JOIN run r USING (sid)
             WHERE e.is_err = 1 ORDER BY e.ts DESC LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, i64, i64, Option<i64>, Option<String>, Option<String>)> = st
        .query_map(params![limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let out: Vec<String> = rows
        .iter()
        .map(|(ext, sid, seq, ts, tool, target)| {
            let s = preview(con, *sid, *seq, 400);
            format!(
                "{{\"ext\":{},\"sid\":{sid},\"seq\":{seq},\"ts\":{},\"tool\":{},\
                  \"target\":{},\"snip\":{}}}",
                jstr(ext), jnum(*ts), jopt(tool), jopt(target), jstr(&s)
            )
        })
        .collect();
    Ok(format!("{{\"rows\":[{}]}}", out.join(",")))
}
