import json
import os
import re

try:
    from orjson import loads
except ImportError:
    from json import loads

_TS = re.compile(r"^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?")
_TARGET = ("file_path", "path", "notebook_path", "url", "pattern")


def ts_ms(v):
    if isinstance(v, (int, float)):
        return int(v * 1000) if v < 1e12 else int(v)
    if not isinstance(v, str):
        return None
    m = _TS.match(v)
    if not m:
        return None
    y, mo, d, h, mi, s, frac = m.groups()
    y, mo, d = int(y), int(mo), int(d)
    y -= mo <= 2
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    doy = (153 * (mo + (-3 if mo > 2 else 9)) + 2) // 5 + d - 1
    days = era * 146097 + yoe * 365 + yoe // 4 - yoe // 100 + doy - 719468
    ms = (((days * 24 + int(h)) * 60 + int(mi)) * 60 + int(s)) * 1000
    return ms + int((frac + "000")[:3]) if frac else ms


def text_of(c):
    if isinstance(c, str):
        return c
    if not isinstance(c, list):
        return ""
    out = []
    for b in c:
        if isinstance(b, str):
            out.append(b)
        elif isinstance(b, dict):
            t = b.get("text")
            if isinstance(t, str):
                out.append(t)
                continue
            inner = b.get("content")
            if isinstance(inner, str):
                out.append(inner)
            elif isinstance(inner, list):
                out.append(text_of(inner))
            elif b.get("type") == "tool_use" and isinstance(b.get("input"), dict):
                out += [v for v in b["input"].values() if isinstance(v, str)]
    return " ".join(x for x in out if x)


def _tool(c):
    tool = target = err = None
    if not isinstance(c, list):
        return tool, target, err
    for b in c:
        if not isinstance(b, dict):
            continue
        if tool is None and b.get("type") == "tool_use":
            tool = b.get("name")
            inp = b.get("input")
            if isinstance(inp, dict):
                for k in _TARGET:
                    if isinstance(inp.get(k), str) and inp[k]:
                        target = inp[k]
                        break
                else:
                    cmd = inp.get("command")
                    target = cmd[:200] if isinstance(cmd, str) else None
        if "is_error" in b:
            err = 1 if b["is_error"] else 0
    return tool, target, err


def _i(v):
    return v if isinstance(v, int) and not isinstance(v, bool) else None


def claude(o):
    m = o.get("message")
    m = m if isinstance(m, dict) else {}
    u = m.get("usage")
    u = u if isinstance(u, dict) else {}
    c = m.get("content")
    tool, target, err = _tool(c)
    return (
        ts_ms(o.get("timestamp")), o.get("type"), m.get("role"), m.get("model"),
        tool, target, _i(u.get("input_tokens")), _i(u.get("output_tokens")),
        _i(u.get("cache_read_input_tokens")), err, text_of(c),
    )


def openai(o):
    c = o.get("content")
    tool, target, err = _tool(c)
    if tool is None and o.get("type") in ("function_call", "tool_call"):
        tool = o.get("name")
    u = o.get("usage")
    u = u if isinstance(u, dict) else {}
    txt = text_of(c) or (o["text"] if isinstance(o.get("text"), str) else "")
    return (
        ts_ms(o.get("created_at") or o.get("timestamp")), o.get("type"), o.get("role"),
        o.get("model"), tool, target, _i(u.get("input_tokens")),
        _i(u.get("output_tokens")), None, err, txt,
    )


def generic(o):
    ts = next((ts_ms(o[k]) for k in ("timestamp", "ts", "time", "created_at") if k in o), None)
    txt = ""
    for k in ("content", "message", "text", "body"):
        v = o.get(k)
        if isinstance(v, str):
            txt = v
            break
        if isinstance(v, (list, dict)):
            txt = text_of(v if isinstance(v, list) else v.get("content"))
            if txt:
                break
    return (
        ts, o.get("type") or o.get("kind") or o.get("event"), o.get("role"), o.get("model"),
        o.get("tool"), None, None, None, None,
        1 if o.get("error") or o.get("is_error") else None, txt,
    )


FMT = {"claude": claude, "codex": claude, "openai": openai, "generic": generic}


def sniff(path, head):
    p = path.lower()
    if ".claude" in p or "/projects/" in p:
        return "claude"
    if ".codex" in p or "/sessions/" in p:
        return "codex"
    if isinstance(head, dict):
        if "parentUuid" in head or "sessionId" in head or isinstance(head.get("message"), dict):
            return "claude"
        if "created_at" in head or head.get("type") in ("message", "function_call"):
            return "openai"
    return "generic"


def ext_id(path, root):
    """Path-derived, never basename: 521 of 10470 corpus files are journal.jsonl."""
    rel = os.path.relpath(path, root)
    return rel[:-6] if rel.endswith(".jsonl") else rel
