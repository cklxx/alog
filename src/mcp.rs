//! MCP over stdio: one JSON-RPC 2.0 line in, one line out.
//!
//! The tools return the same TSV the CLI prints, deliberately. MCP content is
//! text either way, and TSV costs 2.4x fewer tokens than JSON for the same rows
//! because no key repeats -- so wrapping the existing `query::*` output is both
//! cheaper for the reader and one code path instead of two.
//!
//! No framework: the protocol surface an agent actually calls is `initialize`,
//! `tools/list` and `tools/call`, which is a match on three strings.

use rusqlite::Connection;
use std::io::{BufRead, Write};
use std::path::Path;

use crate::query;
use crate::store;

/// Minimal JSON string escaping, same as serve.rs: building a serde_json Value
/// per response costs more than writing the bytes.
fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Read one string field out of a flat JSON object without a parser. The values
/// we need -- a query, a session id, an integer -- are scalars at a known key,
/// and MCP arguments are one level deep.
fn field(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let mut at = json.find(&pat)? + pat.len();
    let b = json.as_bytes();
    while at < b.len() && (b[at] as char).is_whitespace() {
        at += 1;
    }
    if at >= b.len() || b[at] != b':' {
        return None;
    }
    at += 1;
    while at < b.len() && (b[at] as char).is_whitespace() {
        at += 1;
    }
    if at >= b.len() {
        return None;
    }
    if b[at] == b'"' {
        let mut out = String::new();
        let mut i = at + 1;
        while i < b.len() {
            match b[i] {
                b'\\' if i + 1 < b.len() => {
                    out.push(match b[i + 1] {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        c => c as char,
                    });
                    i += 2;
                }
                b'"' => return Some(out),
                c => {
                    // Re-decode UTF-8: pushing bytes as chars would mangle it.
                    let start = i;
                    let len = if c < 0x80 {
                        1
                    } else if c >> 5 == 0b110 {
                        2
                    } else if c >> 4 == 0b1110 {
                        3
                    } else {
                        4
                    };
                    let end = (start + len).min(b.len());
                    out.push_str(&String::from_utf8_lossy(&b[start..end]));
                    i = end;
                }
            }
        }
        None
    } else {
        let start = at;
        while at < b.len() && !matches!(b[at], b',' | b'}' | b']' | b' ') {
            at += 1;
        }
        Some(json[start..at].to_string())
    }
}

fn num(json: &str, key: &str, dflt: usize) -> usize {
    field(json, key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(dflt)
}

/// name, description, and the JSON Schema for arguments.
///
/// Descriptions are written for a model deciding whether to call the tool, and
/// say what the data IS -- the reader has never seen this store and cannot infer
/// that a "session" is a path-derived id or that tokens are per-call.
const TOOLS: &[(&str, &str, &str)] = &[
    (
        "search",
        "Full-text search across every indexed agent session (Claude Code and Codex CLI). \
         fts5 syntax: bare terms, \"exact phrase\", A AND B, A OR B, NOT B, prefix*. Returns TSV \
         with a match snippet read from the source file. Prefer several narrow searches over one \
         broad one; the header reports the total so you know when to narrow.",
        r#"{"type":"object","properties":{"query":{"type":"string","description":"fts5 query"},"limit":{"type":"integer","description":"max rows, default 20"}},"required":["query"]}"#,
    ),
    (
        "catalog",
        "Describe the whole store in ~1.7 KB: schema, time range, per-column null rate, and the \
         full kind/tool/model distributions. Call this first -- it is cheaper than guessing what \
         is in here, and it names the exact values later filters can use.",
        r#"{"type":"object","properties":{}}"#,
    ),
    (
        "turns",
        "One line per user request in a session: what was asked, how many records and tool calls \
         it took, how many failed, and what it cost in tokens. Tokens are counted once per LLM \
         call, not once per record -- a naive sum inflates them 1.7x.",
        r#"{"type":"object","properties":{"session":{"type":"string","description":"session id, as printed by search or catalog"},"limit":{"type":"integer"}},"required":["session"]}"#,
    ),
    (
        "timeline",
        "A session's events in order: kind, role, tool, target, error flag, and a text preview \
         read from the source. Use after `turns` to see how one request actually went.",
        r#"{"type":"object","properties":{"session":{"type":"string"},"start":{"type":"integer","description":"first seq, default 0"},"limit":{"type":"integer"}},"required":["session"]}"#,
    ),
    (
        "errors",
        "Failed tool calls across all sessions, newest first, each naming the tool and what it \
         was acting on. The tool name is inherited from the preceding call when the failure \
         record does not carry one.",
        r#"{"type":"object","properties":{"limit":{"type":"integer"}}}"#,
    ),
    (
        "sql",
        "Read-only SQL over the index. Tables: run(sid,ext,path,size,cursor,n_ev), \
         ev(sid,seq,off,len,ts,kind,role,model,tool,target,in_tok,out_tok,cache_r,is_err), \
         ftx (fts5, MATCH only), ftx_map(rid,sid,seq). ts is epoch ms. Writes and ATTACH are \
         refused. Errors name the fix, including a candidate column or literal.",
        r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]}"#,
    ),
    (
        "record",
        "The original bytes of one record, exactly as the framework wrote them. Verified by \
         checksum: if the source changed on disk this fails loudly rather than returning the \
         wrong bytes.",
        r#"{"type":"object","properties":{"session":{"type":"string"},"seq":{"type":"integer"}},"required":["session","seq"]}"#,
    ),
    (
        "doctor",
        "Verify the index against the files it was built from: re-hash records, find sessions \
         whose file is gone, check the full-text index for desync. Use when a result looks stale \
         -- SQLite's own integrity_check cannot see any of this.",
        r#"{"type":"object","properties":{}}"#,
    ),
];

fn tools_list() -> String {
    let mut out = String::from("{\"tools\":[");
    for (i, (name, desc, schema)) in TOOLS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":{},\"description\":{},\"inputSchema\":{}}}",
            js(name),
            js(desc),
            schema
        ));
    }
    out.push_str("]}");
    out
}

fn sid_of(con: &Connection, ext: &str) -> Result<i64, String> {
    con.query_row(
        "SELECT sid FROM run WHERE ext=?1",
        rusqlite::params![ext],
        |r| r.get(0),
    )
    .map_err(|_| format!("no session {ext}; list them with sql \"SELECT ext FROM run\""))
}

/// Run one tool. Err is a message for the agent, delivered as isError content
/// rather than a protocol-level error -- the call reached the tool, so the model
/// should see the reason and retry differently.
fn call(con: &Connection, name: &str, args: &str) -> Result<String, String> {
    let lim = |d: usize| num(args, "limit", d);
    match name {
        "search" => {
            let q = field(args, "query").ok_or("search needs a query")?;
            Ok(query::search(con, &q, lim(20), true))
        }
        "catalog" => query::catalog(con).map_err(|e| e.to_string()),
        "turns" => {
            let s = field(args, "session").ok_or("turns needs a session")?;
            Ok(query::turns(con, &s, lim(60)))
        }
        "timeline" => {
            let s = field(args, "session").ok_or("timeline needs a session")?;
            Ok(query::outline(
                con,
                &s,
                num(args, "start", 0) as i64,
                lim(80),
            ))
        }
        "errors" => {
            let n = lim(50);
            Ok(query::query(con, &crate::errors_sql(n), n))
        }
        "sql" => {
            let q = field(args, "query").ok_or("sql needs a query")?;
            Ok(query::query(con, &q, lim(50)))
        }
        "record" => {
            let s = field(args, "session").ok_or("record needs a session")?;
            let seq = num(args, "seq", usize::MAX);
            if seq == usize::MAX {
                return Err("record needs a seq".into());
            }
            let sid = sid_of(con, &s)?;
            let raw = store::read_record(con, sid, seq as i64)?;
            Ok(String::from_utf8_lossy(&raw).into_owned())
        }
        "doctor" => Ok(query::doctor(con, 1, false).1),
        other => Err(format!(
            "unknown tool {other}; call tools/list for the {} available",
            TOOLS.len()
        )),
    }
}

/// One JSON-RPC line per message, until stdin closes.
pub fn run(db: &Path) -> Result<(), String> {
    let con = store::open_ro(db).map_err(|e| e.to_string())?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let method = field(line, "method").unwrap_or_default();
        // A notification has no id and must get no response.
        let id = raw_id(line);

        let body = match method.as_str() {
            "initialize" => Some(format!(
                "{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{\"tools\":{{}}}},\
                 \"serverInfo\":{{\"name\":\"alog\",\"version\":{}}}}}",
                js(env!("CARGO_PKG_VERSION"))
            )),
            "tools/list" => Some(tools_list()),
            "tools/call" => {
                let name = field(line, "name").unwrap_or_default();
                let args = args_of(line);
                Some(match call(&con, &name, &args) {
                    // query.rs reports a bad query or a refused write inside its
                    // returned text, as an ALOG_ envelope. That is still a failed
                    // call, so flag it rather than handing the model an error
                    // body labelled success.
                    Ok(text) => format!(
                        "{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]{}}}",
                        js(&text),
                        if text.starts_with("{\"code\":\"ALOG_") {
                            ",\"isError\":true"
                        } else {
                            ""
                        }
                    ),
                    Err(e) => format!(
                        "{{\"content\":[{{\"type\":\"text\",\"text\":{}}}],\"isError\":true}}",
                        js(&e)
                    ),
                })
            }
            "ping" => Some("{}".to_string()),
            _ => None,
        };

        let Some(id) = id else { continue };
        let msg = match body {
            Some(b) => format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{b}}}"),
            None => format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\
                 \"message\":{}}}}}",
                js(&format!("method not found: {method}"))
            ),
        };
        if writeln!(out, "{msg}").is_err() || out.flush().is_err() {
            break;
        }
    }
    Ok(())
}

/// The id verbatim, so a string id stays a string and a number stays a number.
/// None means notification: no reply.
fn raw_id(json: &str) -> Option<String> {
    let at = json.find("\"id\"")? + 4;
    let b = json.as_bytes();
    let mut i = at;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i += 1;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    let start = i;
    if b[i] == b'"' {
        i += 1;
        while i < b.len() && b[i] != b'"' {
            i += 1;
        }
        i += 1;
    } else {
        while i < b.len() && !matches!(b[i], b',' | b'}') {
            i += 1;
        }
    }
    let s = json[start..i].trim();
    if s == "null" || s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// The `arguments` object, so a key named like a field elsewhere in the envelope
/// cannot be read by mistake.
fn args_of(json: &str) -> String {
    let Some(at) = json.find("\"arguments\"") else {
        return String::new();
    };
    let b = json.as_bytes();
    let mut i = at + 11;
    while i < b.len() && b[i] != b'{' {
        if b[i] == b',' {
            return String::new();
        }
        i += 1;
    }
    let start = i;
    let mut depth = 0i32;
    let mut in_str = false;
    while i < b.len() {
        match b[i] {
            b'"' if i == 0 || b[i - 1] != b'\\' => in_str = !in_str,
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return json[start..=i].to_string();
                }
            }
            _ => {}
        }
        i += 1;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_reads_scalars_and_leaves_the_envelope_alone() {
        let j = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"search","arguments":{"query":"a \"quoted\" term","limit":5}}}"#;
        assert_eq!(field(j, "method").as_deref(), Some("tools/call"));
        assert_eq!(field(j, "name").as_deref(), Some("search"));
        let a = args_of(j);
        assert_eq!(field(&a, "query").as_deref(), Some("a \"quoted\" term"));
        assert_eq!(num(&a, "limit", 20), 5);
        // A key that only exists in the envelope must not leak into arguments.
        assert_eq!(field(&a, "method"), None);
    }

    #[test]
    fn id_keeps_its_json_type_and_notifications_get_none() {
        assert_eq!(raw_id(r#"{"id":7,"method":"ping"}"#).as_deref(), Some("7"));
        assert_eq!(
            raw_id(r#"{"id":"abc","method":"ping"}"#).as_deref(),
            Some("\"abc\"")
        );
        assert_eq!(raw_id(r#"{"method":"notifications/initialized"}"#), None);
        assert_eq!(raw_id(r#"{"id":null,"method":"x"}"#), None);
    }

    #[test]
    fn utf8_survives_a_round_trip() {
        let j = r#"{"params":{"arguments":{"query":"重写 fsync"}}}"#;
        assert_eq!(
            field(&args_of(j), "query").as_deref(),
            Some("重写 fsync"),
            "multi-byte characters must not be split"
        );
    }

    #[test]
    fn every_tool_declares_a_schema_and_a_description() {
        for (name, desc, schema) in TOOLS {
            assert!(!name.is_empty());
            assert!(
                desc.len() > 40,
                "{name} needs a description a model can use"
            );
            assert!(schema.starts_with('{'), "{name} schema must be an object");
        }
        // tools/list must be parseable JSON-ish: balanced braces at minimum.
        let l = tools_list();
        assert_eq!(
            l.chars().filter(|c| *c == '{').count(),
            l.chars().filter(|c| *c == '}').count()
        );
    }
}
