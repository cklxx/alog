use rusqlite::Connection;
use crate::store::read_record;
use std::fmt::Write as _;

pub const COLS: &[&str] = &[
    "sid", "seq", "off", "len", "ts", "kind", "role", "model", "tool", "target", "in_tok",
    "out_tok", "cache_r", "is_err",
];

/// Rows an agent or a human reads. 44.2% of the corpus is framework
/// bookkeeping, so this filter runs in SQL, not in the client.
pub const KEEP: &str = "(is_err = 1 OR tool IS NOT NULL OR role IS NOT NULL
     OR kind IN ('assistant','user','system','result','failed','tool_result'))";

fn cell(v: &rusqlite::types::ValueRef<'_>, cap: usize) -> String {
    use rusqlite::types::ValueRef as V;
    let s = match v {
        V::Null => return String::new(),
        V::Integer(i) => i.to_string(),
        V::Real(f) => f.to_string(),
        V::Text(t) => String::from_utf8_lossy(t).into_owned(),
        V::Blob(b) => format!("<{} bytes>", b.len()),
    };
    let s = s.replace('\t', " ").replace('\n', "\\n");
    if s.chars().count() <= cap {
        s
    } else {
        let t: String = s.chars().take(cap).collect();
        format!("{t}…(+{}B)", s.len() - t.len())
    }
}

/// Measured at 339 tokens on a 2.77M-record store: an agent cannot spend 10k
/// tokens learning a schema before asking its first question.
pub fn catalog(con: &Connection) -> rusqlite::Result<String> {
    let (n, runs, rej): (i64, i64, i64) = con.query_row(
        "SELECT (SELECT count(*) FROM ev), (SELECT count(*) FROM run),
                (SELECT count(*) FROM reject)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut out = format!("records={n} sessions={runs} rejects={rej}\n");
    if n == 0 {
        out.push_str("empty; run `alog sync <dir>`");
        return Ok(out);
    }
    let (lo, hi): (Option<i64>, Option<i64>) =
        con.query_row("SELECT min(ts), max(ts) FROM ev", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    if let (Some(a), Some(b)) = (lo, hi) {
        let _ = writeln!(out, "time: {} .. {}", crate::fmt_ts(a), crate::fmt_ts(b));
    }
    let _ = writeln!(out, "cols: {}   (ts epoch_ms)", COLS.join(" "));

    let probe = &COLS[4..];
    let sql = format!(
        "SELECT {} FROM ev",
        probe
            .iter()
            .map(|c| format!("100.0*sum({c} IS NULL)/count(*)"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let nulls: Vec<f64> = con.query_row(&sql, [], |r| {
        (0..probe.len()).map(|i| r.get(i)).collect()
    })?;
    out.push_str("null%:");
    for (c, v) in probe.iter().zip(&nulls) {
        let _ = write!(out, " {c} {v:.0}");
    }
    out.push('\n');

    for col in ["kind", "tool", "model"] {
        let total: i64 = con.query_row(
            &format!("SELECT count(DISTINCT {col}) FROM ev"),
            [],
            |r| r.get(0),
        )?;
        if total == 0 {
            continue;
        }
        let mut st = con.prepare(&format!(
            "SELECT {col}, count(*) FROM ev WHERE {col} IS NOT NULL
             GROUP BY 1 ORDER BY 2 DESC LIMIT 25"
        ))?;
        let rows: Vec<(String, i64)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .flatten()
            .collect();
        let _ = write!(out, "{col}({total}");
        if total as usize > rows.len() {
            let _ = write!(out, " top{}", rows.len());
        }
        out.push_str("):");
        for (k, v) in rows {
            let _ = write!(out, " {k}:{v}");
        }
        out.push('\n');
    }
    Ok(out)
}

/// Agent-authored SQL. TSV, not JSON: the same 20 rows cost 2.4x fewer tokens
/// as TSV because no key is repeated per row.
pub fn query(con: &Connection, sql: &str, limit: usize) -> String {
    let t0 = std::time::Instant::now();
    let mut st = match con.prepare(sql) {
        Ok(s) => s,
        Err(e) => return sql_error(con, &e.to_string()),
    };
    let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let ncol = names.len();
    let mut rows = match st.query([]) {
        Ok(r) => r,
        Err(e) => return sql_error(con, &e.to_string()),
    };
    let mut body = Vec::new();
    let mut more = false;
    loop {
        match rows.next() {
            Ok(Some(r)) => {
                if body.len() == limit {
                    more = true;
                    break;
                }
                let line: Vec<String> = (0..ncol)
                    .map(|i| cell(&r.get_ref_unwrap(i), 300))
                    .collect();
                body.push(line.join("\t"));
            }
            Ok(None) => break,
            Err(e) => return sql_error(con, &e.to_string()),
        }
    }
    let mut out = format!("rows={} elapsed={}ms", body.len(), t0.elapsed().as_millis());
    if more {
        let _ = write!(out, " TRUNCATED at limit={limit}, more rows exist");
    }
    out.push('\n');
    out.push_str(&names.join("\t"));
    for b in &body {
        out.push('\n');
        out.push_str(b);
    }
    if body.is_empty() {
        if let Some(h) = no_rows_hint(con, sql) {
            out.push('\n');
            out.push_str(&h);
        }
    }
    out
}

pub fn search(con: &Connection, term: &str, limit: usize, snippets: bool) -> String {
    let t0 = std::time::Instant::now();
    let mut st = match con.prepare(
        "SELECT r.ext, m.sid, m.seq, e.ts, e.kind, e.tool, e.target FROM ftx
         JOIN ftx_map m ON m.rid = ftx.rowid
         JOIN ev e ON e.sid = m.sid AND e.seq = m.seq
         JOIN run r ON r.sid = m.sid
         WHERE ftx MATCH ?1 ORDER BY rank LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(e) => return fts_error(&e.to_string()),
    };
    let hits: Vec<(String, i64, i64, Option<i64>, Option<String>, Option<String>, Option<String>)> =
        match st.query_map(rusqlite::params![term, limit as i64 + 1], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        }) {
            Ok(it) => it.flatten().collect(),
            Err(e) => return fts_error(&e.to_string()),
        };
    let more = hits.len() > limit;
    let hits = &hits[..hits.len().min(limit)];
    let mut out = format!("hits={} elapsed={}ms", hits.len(), t0.elapsed().as_millis());
    if more {
        let _ = write!(out, " TRUNCATED at limit={limit}");
    }
    if hits.is_empty() {
        out.push_str("\nno matches; try a shorter term or pre*");
        return out;
    }
    let words = terms_of(term);
    out.push_str("\nsession\tseq\twhen\tkind\ttool\ttarget");
    if snippets {
        out.push_str("\tmatch");
    }
    for (ext, sid, seq, ts, kind, tool, target) in hits {
        let _ = write!(
            out,
            "\n{}\t{}\t{}\t{}\t{}\t{}",
            trunc(ext, 60),
            seq,
            ts.map(crate::fmt_ts).unwrap_or_default(),
            kind.as_deref().unwrap_or(""),
            tool.as_deref().unwrap_or(""),
            trunc(target.as_deref().unwrap_or(""), 60)
        );
        if snippets {
            let s = crate::store::read_record(con, *sid, *seq)
                .map(|b| snip(&String::from_utf8_lossy(&b), &words, 240))
                .unwrap_or_default();
            let _ = write!(out, "\t{}", s.replace('\t', " ").replace('\n', "\\n"));
        }
    }
    out
}

/// ~9 tokens per event, so an 80-step window is ~720 tokens.
pub fn outline(con: &Connection, ext: &str, start: i64, limit: usize) -> String {
    let found: rusqlite::Result<(i64, i64)> = con.query_row(
        "SELECT sid, n_ev FROM run WHERE ext=?1",
        rusqlite::params![ext],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let (sid, n_ev) = match found {
        Ok(v) => v,
        Err(_) => {
            let mut st = match con.prepare("SELECT ext FROM run LIMIT 2000") {
                Ok(s) => s,
                Err(e) => return e.to_string(),
            };
            let all: Vec<String> = st
                .query_map([], |r| r.get(0))
                .map(|i| i.flatten().collect())
                .unwrap_or_default();
            let near = closest(ext, all.iter().map(|s| s.as_str()), 5);
            return format!(
                "{{\"code\":\"ALOG_UNKNOWN_SESSION\",\"message\":{:?},\"candidates\":{:?}}}",
                ext, near
            );
        }
    };
    let mut st = match con.prepare(&format!(
        "SELECT seq, ts, kind, role, tool, target, is_err, len FROM ev
         WHERE sid=?1 AND seq>=?2 AND {KEEP} ORDER BY seq LIMIT ?3"
    )) {
        Ok(s) => s,
        Err(e) => return e.to_string(),
    };
    let rows: Vec<(i64, Option<i64>, Option<String>, Option<String>, Option<String>, Option<String>, Option<i64>, i64)> =
        st.query_map(rusqlite::params![sid, start, limit as i64], |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                r.get(7)?,
            ))
        })
        .map(|i| i.flatten().collect())
        .unwrap_or_default();
    let mut out = format!("session={ext} events={n_ev}");
    if let Some((last, _, _, _, _, _, _, _)) = rows.last() {
        if *last + 1 < n_ev {
            let _ = write!(out, " (more: start={})", last + 1);
        }
    }
    out.push_str("\nseq\twhen\tkind\trole\ttool\terr\tbytes\twhat");
    for (seq, ts, kind, role, tool, target, err, len) in rows {
        // Same fallback the viewer needs: without it 121 of 400 rows on a real
        // session print as a bare role and nothing else.
        let what = match target {
            Some(t) => trunc(&t, 90),
            None => trunc(&preview(con, sid, seq, 90), 90),
        };
        let _ = write!(
            out,
            "\n{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            seq,
            ts.map(crate::fmt_ts).unwrap_or_default(),
            kind.as_deref().unwrap_or(""),
            role.as_deref().unwrap_or(""),
            tool.as_deref().unwrap_or(""),
            if err == Some(1) { "ERR" } else { "" },
            len,
            what
        );
    }
    out
}

// ------------------------------------------------------------ errors

/// An error a model can act on in one turn: the failing element, the valid
/// alternatives, and whether the fix is mechanical.
fn sql_error(con: &Connection, msg: &str) -> String {
    let low = msg.to_lowercase();
    let bad_token = |msg: &str| -> String {
        // rusqlite: "no such column: toolz in SELECT toolz FROM ev at offset 7"
        msg.split(':')
            .nth(1)
            .unwrap_or("")
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    };
    let (code, cands) = if low.contains("no such column") {
        let bad = bad_token(msg);
        let bad = bad.as_str();
        (
            "ALOG_UNKNOWN_COLUMN",
            closest(bad, COLS.iter().copied(), 4),
        )
    } else if low.contains("no such table") {
        let bad = bad_token(msg);
        let bad = bad.as_str();
        let mut tables = Vec::new();
        if let Ok(mut st) =
            con.prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view')")
        {
            if let Ok(it) = st.query_map([], |r| r.get::<_, String>(0)) {
                tables = it.flatten().collect();
            }
        }
        let near = closest(bad, tables.iter().map(|s| s.as_str()), 4);
        (
            "ALOG_UNKNOWN_TABLE",
            if near.is_empty() { tables } else { near },
        )
    } else if low.contains("not authorized") {
        ("ALOG_DENIED", vec![])
    } else {
        ("ALOG_SQL_ERROR", vec![])
    };
    format!(
        "{{\"code\":\"{code}\",\"message\":{:?},\"candidates\":{:?},\"applicability\":\"{}\"}}",
        msg,
        cands,
        if cands.is_empty() {
            "Unspecified"
        } else {
            "MachineApplicable"
        }
    )
}

fn fts_error(msg: &str) -> String {
    format!(
        "{{\"code\":\"ALOG_BAD_QUERY\",\"message\":{:?},\
         \"syntax\":\"term, \\\"exact phrase\\\", A AND B, A OR B, NOT B, pre*\"}}",
        msg
    )
}

/// Zero rows is usually a wrong literal, not an empty store.
fn no_rows_hint(con: &Connection, sql: &str) -> Option<String> {
    let mut hints = Vec::new();
    let lower = sql.to_lowercase();
    for col in COLS {
        for pat in [format!("{col}="), format!("{col} =")] {
            if let Some(i) = lower.find(&pat) {
                let rest = &sql[i + pat.len()..];
                let lit = rest
                    .trim_start()
                    .strip_prefix('\'')
                    .and_then(|r| r.split('\'').next());
                if let Some(lit) = lit {
                    let mut st = con
                        .prepare(&format!(
                            "SELECT DISTINCT {col} FROM ev WHERE {col} IS NOT NULL LIMIT 500"
                        ))
                        .ok()?;
                    let vals: Vec<String> = st
                        .query_map([], |r| r.get::<_, String>(0))
                        .ok()?
                        .flatten()
                        .collect();
                    if !vals.iter().any(|v| v == lit) {
                        let near = closest(lit, vals.iter().map(|s| s.as_str()), 3);
                        if !near.is_empty() {
                            hints.push(format!("{col}: {}", near.join(", ")));
                        }
                    }
                }
                break;
            }
        }
    }
    if hints.is_empty() {
        None
    } else {
        Some(format!(
            "matched 0 rows. did_you_mean -> {}",
            hints.join("; ")
        ))
    }
}

// ------------------------------------------------------------ helpers

fn trunc(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let t: String = s.chars().take(cap).collect();
    format!("{t}…")
}

pub fn terms_of(q: &str) -> Vec<String> {
    q.replace(['"', '*', '.', '(', ')'], " ")
        .split_whitespace()
        .filter(|w| !matches!(w.to_uppercase().as_str(), "AND" | "OR" | "NOT" | "NEAR"))
        .map(|s| s.to_string())
        .collect()
}

pub fn snip(s: &str, terms: &[String], width: usize) -> String {
    let low = s.to_lowercase();
    // to_lowercase can change byte length, so map the hit back by counting
    // chars rather than trusting the byte offset.
    let at = terms
        .iter()
        .filter_map(|t| low.find(&t.to_lowercase()))
        .min()
        .map(|b| low[..b].chars().count());
    let skip = at.map_or(0, |c| c.saturating_sub(width / 3));
    let out: String = s.chars().skip(skip).take(width).collect();
    if skip > 0 {
        format!("…{out}")
    } else {
        out
    }
}

/// Levenshtein-ratio nearest strings, the "did you mean" behind every error.
fn closest<'a>(want: &str, pool: impl Iterator<Item = &'a str>, n: usize) -> Vec<String> {
    let mut scored: Vec<(usize, &str)> = pool
        .filter_map(|c| {
            let d = lev(&want.to_lowercase(), &c.to_lowercase());
            let max = want.len().max(c.len()).max(1);
            (d * 2 <= max).then_some((d, c))
        })
        .collect();
    scored.sort_by_key(|(d, c)| (*d, c.len()));
    scored.into_iter().take(n).map(|(_, c)| c.to_string()).collect()
}

fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ----------------------------------------------------------- preview

/// First line of readable text from the source record.
pub fn preview(con: &Connection, sid: i64, seq: i64, cap: usize) -> String {
    let raw = match read_record(con, sid, seq) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let v: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return String::from_utf8_lossy(&raw[..raw.len().min(cap)]).into_owned(),
    };
    let msg = v.get("message").filter(|m| m.is_object()).unwrap_or(&v);
    let mut buf = String::new();
    collect_text(msg.get("content").unwrap_or(&serde_json::Value::Null), &mut buf);
    // A `system` record has no message.content at all; its subtype is the only
    // human-readable field. A redacted thinking block leaves an empty string
    // behind, and 77 of 400 timeline rows were that -- all with real token
    // counts, so they are dropped from neither the index nor the view.
    if buf.is_empty() {
        if let Some(s) = v.get("subtype").and_then(|s| s.as_str()) {
            buf.push_str(s);
        } else if has_thinking(msg.get("content")) {
            buf.push_str("(thinking)");
        }
    }
    let flat: String = buf.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > cap {
        let t: String = flat.chars().take(cap).collect();
        format!("{t}…")
    } else {
        flat
    }
}

fn has_thinking(v: Option<&serde_json::Value>) -> bool {
    v.and_then(|c| c.as_array()).is_some_and(|a| {
        a.iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking"))
    })
}

fn collect_text(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(s);
        }
        serde_json::Value::Array(a) => {
            for x in a {
                collect_text(x, out);
            }
        }
        serde_json::Value::Object(m) => {
            // `thinking` before `text`: a thinking block carries no text key, and
            // a whole assistant turn can be nothing but thinking.
            for k in ["text", "thinking", "content"] {
                if let Some(x) = m.get(k) {
                    collect_text(x, out);
                    return;
                }
            }
            if m.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(serde_json::Value::Object(inp)) = m.get("input") {
                    for x in inp.values() {
                        if x.is_string() {
                            collect_text(x, out);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}
