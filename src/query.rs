use crate::store::read_record;
use rusqlite::Connection;
use std::fmt::Write as _;

pub const COLS: &[&str] = &[
    "sid", "seq", "off", "len", "ts", "kind", "role", "model", "tool", "target", "in_tok",
    "out_tok", "cache_r", "is_err",
];

/// Rows an agent or a human reads. 44.2% of the corpus is framework
/// bookkeeping, so this filter runs in SQL, not in the client. The kind list is
/// Claude Code's; Codex kinds are namespaced, so its content records match on
/// the prefix and only its noisiest event (token_count) is dropped.
pub const KEEP: &str = "(is_err = 1 OR tool IS NOT NULL OR role IS NOT NULL
     OR kind IN ('assistant','user','system','result','failed','tool_result')
     OR (kind LIKE 'response_item/%' AND kind != 'response_item/reasoning')
     OR kind IN ('event_msg/user_message','event_msg/agent_message',
                 'event_msg/agent_reasoning','event_msg/error'))";

type Nullable = Option<String>;
/// (ext, sid, seq, ts, kind, tool, target)
type Hit = (String, i64, i64, Option<i64>, Nullable, Nullable, Nullable);
/// (turn, first_seq, t0, t1, records, tools, errors, calls, in, out, cache)
type Turn = (
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
);
/// (seq, ts, kind, role, tool, target, is_err, len)
type Step = (
    i64,
    Option<i64>,
    Nullable,
    Nullable,
    Nullable,
    Nullable,
    Option<i64>,
    i64,
);

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
    let nulls: Vec<f64> =
        con.query_row(&sql, [], |r| (0..probe.len()).map(|i| r.get(i)).collect())?;
    out.push_str("null%:");
    for (c, v) in probe.iter().zip(&nulls) {
        let _ = write!(out, " {c} {v:.0}");
    }
    out.push('\n');

    for col in ["kind", "tool", "model"] {
        let total: i64 =
            con.query_row(&format!("SELECT count(DISTINCT {col}) FROM ev"), [], |r| {
                r.get(0)
            })?;
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
                let line: Vec<String> =
                    (0..ncol).map(|i| cell(&r.get_ref_unwrap(i), 300)).collect();
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
    let hits: Vec<Hit> = match st.query_map(rusqlite::params![term, limit as i64 + 1], |r| {
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
    // The total, not just the fact of truncation: "20 of 4184" tells a reader
    // whether to narrow the query, where "TRUNCATED" only says something was
    // hidden. Costs one more fts5 count -- measured 0.03 ms on a term with 294
    // hits and 6.5 ms on one with 392,758, against a ~50 ms process floor.
    let total: i64 = if more {
        con.query_row(
            "SELECT count(*) FROM ftx WHERE ftx MATCH ?1",
            rusqlite::params![term],
            |r| r.get(0),
        )
        .unwrap_or(-1)
    } else {
        hits.len() as i64
    };
    let mut out = if more && total > 0 {
        format!("hits={} of {total}", hits.len())
    } else {
        format!("hits={}", hits.len())
    };
    let _ = write!(out, " elapsed={}ms", t0.elapsed().as_millis());
    if more {
        let _ = if total > 0 {
            write!(out, " -- narrow the query or raise --limit")
        } else {
            write!(out, " TRUNCATED at limit={limit}")
        };
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
            // A read failure is named, never blanked. An empty column here read
            // as "the match is not quotable", when it meant the source no longer
            // holds this record and the hit itself is stale.
            let s = match crate::store::read_record(con, *sid, *seq) {
                Ok(b) => snip(&String::from_utf8_lossy(&b), &words, 240),
                Err(e) => format!("<{e}>"),
            };
            let _ = write!(out, "\t{}", s.replace('\t', " ").replace('\n', "\\n"));
        }
    }
    out
}

/// A turn is one user request and everything the agent did to answer it.
/// Neither format marks turns (DSH does: turn/start, step/start, callId), so
/// the boundary is derived per format:
///   Claude Code -- kind 'user' with is_err NULL. A tool result always carries
///     is_err 0 or 1, measured 1,397 real prompts against 15,416 tool results.
///   Codex -- kind 'event_msg/user_message', which is explicit.
/// A turn only counts if an assistant record follows, which drops the injected
/// pseudo-prompts Claude Code writes before the model ever runs.
///
/// Tokens are counted once per LLM call, not once per record. One call emits a
/// record per content block (reasoning, text, tool_use) and copies the whole
/// usage object onto each, so summing naively inflates the total: measured
/// 29,527 assistant records against 18,126 distinct usages corpus-wide, and on
/// one session 2,004,301 output tokens claimed against 961,499 real.
pub const TURNS: &str = "WITH t AS (
    SELECT seq, ts, kind, tool, is_err, in_tok, out_tok, cache_r,
           sum(kind = 'event_msg/user_message'
               OR (kind = 'user' AND is_err IS NULL)) OVER (ORDER BY seq) AS turn,
           lag(in_tok)  OVER (ORDER BY seq) AS pi,
           lag(out_tok) OVER (ORDER BY seq) AS po,
           lag(cache_r) OVER (ORDER BY seq) AS pc
    FROM ev WHERE sid = ?1
), u AS (
    SELECT *, (out_tok IS NOT NULL AND (
                 kind = 'event_msg/token_count'
                 OR in_tok IS NOT pi OR out_tok IS NOT po OR cache_r IS NOT pc
               )) AS newcall
    FROM t
) SELECT turn, min(seq), min(ts), max(ts), count(*), sum(tool IS NOT NULL),
         sum(is_err = 1), sum(newcall),
         sum(CASE WHEN newcall THEN coalesce(in_tok,0)  ELSE 0 END),
         sum(CASE WHEN newcall THEN coalesce(out_tok,0) ELSE 0 END),
         sum(CASE WHEN newcall THEN coalesce(cache_r,0) ELSE 0 END)
  FROM u WHERE turn > 0 GROUP BY turn
  HAVING sum(kind = 'assistant' OR kind LIKE 'response_item/%'
             OR kind = 'event_msg/agent_message') > 0
  ORDER BY turn";

fn session(con: &Connection, ext: &str) -> Result<(i64, i64), String> {
    match con.query_row(
        "SELECT sid, n_ev FROM run WHERE ext=?1",
        rusqlite::params![ext],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ) {
        Ok(v) => Ok(v),
        Err(_) => {
            let mut st = con
                .prepare("SELECT ext FROM run LIMIT 2000")
                .map_err(|e| e.to_string())?;
            let all: Vec<String> = st
                .query_map([], |r| r.get(0))
                .map(|i| i.flatten().collect())
                .unwrap_or_default();
            Err(format!(
                "{{\"code\":\"ALOG_UNKNOWN_SESSION\",\"message\":{:?},\"candidates\":{:?}}}",
                ext,
                closest(ext, all.iter().map(|s| s.as_str()), 5)
            ))
        }
    }
}

/// One line per turn: what was asked, how much work it took, what it cost.
pub fn turns(con: &Connection, ext: &str, limit: usize) -> String {
    let (sid, _) = match session(con, ext) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut st = match con.prepare(TURNS) {
        Ok(s) => s,
        Err(e) => return e.to_string(),
    };
    let rows: Vec<Turn> = st
        .query_map(rusqlite::params![sid], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        })
        .map(|i| i.flatten().collect())
        .unwrap_or_default();
    let mut out = format!("session={ext} turns={}", rows.len());
    out.push_str("\nturn\twhen\tmins\tev\tcalls\ttools\terr\tin\tout\tcache\task");
    for (i, (_, s0, t0, t1, n, tools, err, calls, itok, otok, ctok)) in
        rows.iter().take(limit).enumerate()
    {
        let mins = match (t0, t1) {
            (Some(a), Some(b)) => format!("{:.0}", (b - a) as f64 / 60000.0),
            _ => String::new(),
        };
        let _ = write!(
            out,
            "\n{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            i + 1,
            t0.map(crate::fmt_ts).unwrap_or_default(),
            mins,
            n,
            calls,
            tools,
            if *err > 0 {
                err.to_string()
            } else {
                String::new()
            },
            itok,
            otok,
            ctok,
            trunc(&preview(con, sid, *s0, 70), 70)
        );
    }
    if rows.len() > limit {
        let _ = write!(
            out,
            "\nTRUNCATED at limit={limit}, {} turns total",
            rows.len()
        );
    }
    out
}

/// ~9 tokens per event, so an 80-step window is ~720 tokens.
pub fn outline(con: &Connection, ext: &str, start: i64, limit: usize) -> String {
    let (sid, n_ev) = match session(con, ext) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut st = match con.prepare(&format!(
        "SELECT seq, ts, kind, role, tool, target, is_err, len FROM ev
         WHERE sid=?1 AND seq>=?2 AND {KEEP} ORDER BY seq LIMIT ?3"
    )) {
        Ok(s) => s,
        Err(e) => return e.to_string(),
    };
    let rows: Vec<Step> = st
        .query_map(rusqlite::params![sid, start, limit as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
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

/// An error a model can act on in one turn: the failing element, the valid
/// alternatives, and whether the fix is mechanical.
fn sql_error(con: &Connection, msg: &str) -> String {
    let low = msg.to_lowercase();
    let bad_token = |msg: &str| -> String {
        // rusqlite: "no such column: toolz in SELECT toolz FROM ev at offset 7"
        msg.split(':')
            .nth(1)
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    };
    let (code, cands) = if low.contains("no such column") {
        let bad = bad_token(msg);
        let bad = bad.as_str();
        ("ALOG_UNKNOWN_COLUMN", closest(bad, COLS.iter().copied(), 4))
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
    scored
        .into_iter()
        .take(n)
        .map(|(_, c)| c.to_string())
        .collect()
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
    collect_text(
        msg.get("content").unwrap_or(&serde_json::Value::Null),
        &mut buf,
    );
    // A `system` record has no message.content; its subtype is the only readable
    // field. A redacted thinking block leaves an empty string. A Codex record
    // keeps everything under `payload`, so reuse the extractor rather than
    // teaching this function a second format.
    if buf.is_empty() {
        if v.get("payload").is_some() {
            if let Some(r) = crate::extract::codex(std::str::from_utf8(&raw).unwrap_or(""), 0, 0, 0)
            {
                buf = if r.text.is_empty() {
                    r.target.unwrap_or_default()
                } else {
                    r.text
                };
            }
        } else if let Some(s) = v.get("subtype").and_then(|s| s.as_str()) {
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

/// Verify the index against the files it was built from, and say what is wrong
/// in a form both a person and an agent can act on.
///
/// `PRAGMA integrity_check` is not this check and cannot replace it: it returns
/// `ok` on an index whose every row answers for text that is no longer on disk,
/// because SQLite's pages are perfectly consistent -- they just describe a file
/// that changed. So this reads sources: it re-hashes a sample of records through
/// the same `rec_hash` the writer used, and stats every indexed path.
///
/// `sample` records are checked per session, newest sessions first. 0 means all.
pub fn doctor(con: &Connection, sample: usize, json: bool) -> (bool, String) {
    let t0 = std::time::Instant::now();
    let mut problems: Vec<(String, String, String)> = Vec::new(); // kind, what, hint

    let (evn, runs, rej): (i64, i64, i64) = con
        .query_row(
            "SELECT (SELECT count(*) FROM ev), (SELECT count(*) FROM run),
                    (SELECT count(*) FROM reject)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or((0, 0, 0));

    let ok: String = con
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap_or_else(|e| e.to_string());
    if ok != "ok" {
        problems.push((
            "corrupt-index".into(),
            format!("integrity_check: {ok}"),
            "rebuild: delete the index and run `alog sync`".into(),
        ));
    }

    // A run row whose file is gone keeps answering searches and keeps skewing
    // the corpus-global bm25 IDF every other session's ranking depends on.
    let paths: Vec<(i64, String, String)> =
        match con.prepare("SELECT sid, ext, path FROM run ORDER BY sid") {
            Ok(mut st) => st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map(|it| it.flatten().collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
    let mut gone: std::collections::HashSet<i64> = Default::default();
    let mut missing = 0usize;
    for (sid, ext, path) in &paths {
        if !std::path::Path::new(path).exists() {
            missing += 1;
            gone.insert(*sid);
            if missing <= 5 {
                problems.push((
                    "file-gone".into(),
                    format!("{ext}: {path}"),
                    "run `alog sync` to drop it".into(),
                ));
            }
        }
    }
    if missing > 5 {
        problems.push((
            "file-gone".into(),
            format!("and {} more", missing - 5),
            "run `alog sync` to drop them".into(),
        ));
    }

    // The real check: do the bytes on disk still hash to what was stored?
    let mut checked = 0usize;
    let mut stale = 0usize;
    for (sid, ext, _) in &paths {
        if gone.contains(sid) {
            continue; // already reported as file-gone
        }
        let sql = if sample == 0 {
            "SELECT seq FROM ev WHERE sid=?1 ORDER BY seq".to_string()
        } else {
            format!("SELECT seq FROM ev WHERE sid=?1 ORDER BY seq DESC LIMIT {sample}")
        };
        let seqs: Vec<i64> = match con.prepare(&sql) {
            Ok(mut st) => st
                .query_map(rusqlite::params![sid], |r| r.get(0))
                .map(|it| it.flatten().collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for seq in seqs {
            checked += 1;
            if let Err(e) = read_record(con, *sid, seq) {
                stale += 1;
                if stale <= 5 {
                    problems.push((
                        "stale-record".into(),
                        format!("{ext} seq={seq}: {e}"),
                        "run `alog sync` to reindex the file".into(),
                    ));
                }
            }
        }
    }
    if stale > 5 {
        problems.push((
            "stale-record".into(),
            format!("and {} more", stale - 5),
            "run `alog sync`".into(),
        ));
    }

    // fts5 holds one document per record with text. A count far below the
    // number of text-bearing rows means documents were lost.
    let (docs, mapped): (i64, i64) = con
        .query_row(
            "SELECT (SELECT count(*) FROM ftx), (SELECT count(*) FROM ftx_map)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    if docs != mapped {
        problems.push((
            "fts-desync".into(),
            format!("{docs} documents but {mapped} map rows"),
            "rebuild: delete the index and run `alog sync`".into(),
        ));
    }
    let orphans: i64 = con
        .query_row(
            "SELECT count(*) FROM ftx_map m
             WHERE NOT EXISTS (SELECT 1 FROM ev e WHERE e.sid=m.sid AND e.seq=m.seq)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if orphans > 0 {
        problems.push((
            "fts-orphan".into(),
            format!("{orphans} documents point at rows that are gone"),
            "rebuild: delete the index and run `alog sync`".into(),
        ));
    }

    let el = t0.elapsed().as_millis();
    if json {
        let mut out = format!(
            "{{\"ok\":{},\"sessions\":{runs},\"records\":{evn},\"rejects\":{rej},\
             \"checked\":{checked},\"stale\":{stale},\"missing_files\":{missing},\
             \"elapsed_ms\":{el},\"problems\":[",
            problems.is_empty()
        );
        for (i, (kind, what, hint)) in problems.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"kind\":{kind:?},\"what\":{what:?},\"hint\":{hint:?}}}"
            );
        }
        out.push_str("]}");
        return (problems.is_empty(), out);
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "sessions {runs}  records {evn}  rejects {rej}\nverified {checked} records against their \
         source files in {el}ms"
    );
    if problems.is_empty() {
        out.push_str("ok");
        return (true, out);
    } else {
        let _ = writeln!(out, "\n{} problem(s):", problems.len());
        for (kind, what, hint) in &problems {
            let _ = writeln!(out, "  [{kind}] {what}\n      -> {hint}");
        }
    }
    (false, out)
}
