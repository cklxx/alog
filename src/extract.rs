// alog scan+extract core. Byte-compatible with alog/extract.py claude_code().
// No SQLite. No threads here; the caller owns parallelism.

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::Value;
use serde_json::Map;
use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

/// Shared with Python. A record whose bracket nesting exceeds this is a reject
/// on BOTH sides, so the two parsers' own limits (serde_json 128, orjson ~510,
/// CPython json ~600) never decide the outcome.
pub const MAX_DEPTH: usize = 100;
pub const MAX_LINE: usize = 32 << 20;
/// Python slices `command[:200]` in CHARACTERS.
pub const CMD_CHARS: usize = 200;

// ---------------------------------------------------------------- strip

/// Exactly CPython `bytes.strip()`: {0x09,0x0a,0x0b,0x0c,0x0d,0x20}.
/// `u8::is_ascii_whitespace` omits 0x0b (vertical tab) and would change both
/// `len` and `rec_hash` on any line wrapped in it.
#[inline]
pub fn strip_py(b: &[u8]) -> &[u8] {
    #[inline(always)]
    fn ws(c: u8) -> bool {
        matches!(c, b' ' | 0x09 | 0x0a | 0x0b | 0x0c | 0x0d)
    }
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && ws(b[s]) {
        s += 1;
    }
    while e > s && ws(b[e - 1]) {
        e -= 1;
    }
    &b[s..e]
}

/// crc32(stripped) | (len(stripped) << 32), matching index.py `_rec_hash`.
#[inline]
pub fn rec_hash(b: &[u8]) -> i64 {
    let c = crc32fast::hash(b) as u64;
    (c | ((b.len() as u64) << 32)) as i64
}

/// Upper bound on nesting: depth can never exceed the number of open brackets,
/// counted without tracking strings. Cheap enough to run on every line.
#[inline]
pub fn bracket_count(b: &[u8]) -> usize {
    memchr::memchr2_iter(b'[', b'{', b).count()
}

/// Max bracket nesting, strings skipped. Decides depth rejects before any
/// parser's own limit can.
pub fn max_depth(b: &[u8]) -> usize {
    let mut d = 0usize;
    let mut mx = 0usize;
    let mut in_s = false;
    let mut esc = false;
    for &c in b {
        if in_s {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_s = false;
            }
            continue;
        }
        match c {
            b'"' => in_s = true,
            b'[' | b'{' => {
                d += 1;
                if d > mx {
                    mx = d;
                }
            }
            b']' | b'}' => d = d.saturating_sub(1),
            _ => {}
        }
    }
    mx
}

// ------------------------------------------------- presence-preserving field

/// Distinguishes an absent key from an explicit `null`. Needed ONLY for
/// `is_error`, which Python tests with `"is_error" in b` (membership), not with
/// `.get()`. Every other field uses `.get()`, where absent and null are
/// indistinguishable, so those map null -> Absent.
#[derive(Debug, Clone, Default)]
pub enum Present {
    #[default]
    Absent,
    Val(Value),
}

impl<'de> Deserialize<'de> for Present {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Present::Val(Value::deserialize(d)?))
    }
}

/// A map key that borrows from the input when it has no escapes. Deserializing
/// keys as `Cow<'de, str>` directly would allocate on EVERY key, because
/// serde's blanket Cow impl always produces Owned.
struct Key<'a>(Cow<'a, str>);

impl<'de: 'a, 'a> Deserialize<'de> for Key<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = Key<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string key")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(Key(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Key(Cow::Owned(v.to_owned())))
            }
        }
        d.deserialize_str(V(PhantomData))
    }
}

/// Hand-written object deserializers. `#[derive(Deserialize)]` is WRONG here on
/// two counts, both silent and both invisible to the corpus:
///   1. it ERRORS on a duplicate key ("duplicate field `cwd`") where CPython
///      dict takes last-wins, rejecting a whole record Python indexes;
///   2. it accepts a struct built FROM A JSON ARRAY by field position, so
///      `[1,2,3]` became a row with a fabricated ts=1000 where Python rejects
///      `not_an_object`.
/// `strict` accepts only a map (used for the top-level record). `lenient`
/// returns Default for any non-map, mirroring `x if isinstance(x, dict) else {}`.
macro_rules! obj_de {
    (@body $name:ident $(<$lt:lifetime>)?, $a:ident, [$($key:literal => $f:ident : $fty:ty),* $(,)?]) => {{
        let mut out = $name::default();
        while let Some(k) = $a.next_key::<Key>()? {
            match k.0.as_ref() {
                // Last write wins, in place: CPython dict and IndexMap agree.
                $($key => out.$f = $a.next_value::<$fty>()?,)*
                _ => { $a.next_value::<IgnoredAny>()?; }
            }
        }
        Ok(out)
    }};
    (strict $name:ident<$lt:lifetime>, [$($key:literal => $f:ident : $fty:ty),* $(,)?]) => {
        impl<'de: $lt, $lt> Deserialize<'de> for $name<$lt> {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V<$lt>(PhantomData<&$lt ()>);
                impl<'de: $lt, $lt> Visitor<'de> for V<$lt> {
                    type Value = $name<$lt>;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("a JSON object")
                    }
                    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                        obj_de!(@body $name<$lt>, a, [$($key => $f : $fty),*])
                    }
                }
                d.deserialize_map(V(PhantomData))
            }
        }
    };
    (lenient $name:ident<$lt:lifetime>, [$($key:literal => $f:ident : $fty:ty),* $(,)?]) => {
        impl<'de: $lt, $lt> Deserialize<'de> for $name<$lt> {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V<$lt>(PhantomData<&$lt ()>);
                impl<'de: $lt, $lt> Visitor<'de> for V<$lt> {
                    type Value = $name<$lt>;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("a JSON object or any other value")
                    }
                    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                        obj_de!(@body $name<$lt>, a, [$($key => $f : $fty),*])
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                        while a.next_element::<IgnoredAny>()?.is_some() {}
                        Ok($name::default())
                    }
                    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                        Self::Value::deserialize(d)
                    }
                    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> { Ok($name::default()) }
                }
                d.deserialize_any(V(PhantomData))
            }
        }
    };
    (lenient $name:ident, [$($key:literal => $f:ident : $fty:ty),* $(,)?]) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("a JSON object or any other value")
                    }
                    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                        obj_de!(@body $name, a, [$($key => $f : $fty),*])
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                        while a.next_element::<IgnoredAny>()?.is_some() {}
                        Ok($name::default())
                    }
                    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                        Self::Value::deserialize(d)
                    }
                    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> { Ok($name::default()) }
                    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> { Ok($name::default()) }
                }
                d.deserialize_any(V)
            }
        }
    };
}

/// A field Python reads with `isinstance(v, str)`: anything else behaves as
/// absent for the value, but is still "present" for control flow.
#[derive(Debug, Clone, Default)]
pub enum MaybeStr<'a> {
    #[default]
    Absent,
    Str(Cow<'a, str>),
    NotStr,
}

impl<'a> MaybeStr<'a> {
    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            MaybeStr::Str(s) => Some(s.as_ref()),
            _ => None,
        }
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for MaybeStr<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = MaybeStr<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            // No escapes: serde_json hands us a slice of the input.
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(MaybeStr::Str(Cow::Borrowed(v)))
            }
            // Escapes or \uXXXX: serde_json must unescape, so we own it. This is
            // the case `#[serde(borrow)] Option<&str>` rejects outright.
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(MaybeStr::Str(Cow::Owned(v.to_owned())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(MaybeStr::Str(Cow::Owned(v)))
            }
            // `null` == absent: Python reads every one of these with .get().
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(MaybeStr::Absent)
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(MaybeStr::Absent)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                MaybeStr::deserialize(d)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(MaybeStr::NotStr)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(MaybeStr::NotStr)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(MaybeStr::NotStr)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(MaybeStr::NotStr)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MaybeStr::NotStr)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(MaybeStr::NotStr)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// A field Python reads with `isinstance(v, dict) else {}`.
fn de_map_or_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    struct V<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de> + Default> Visitor<'de> for V<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a JSON object or anything else")
        }
        fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<T, A::Error> {
            T::deserialize(de::value::MapAccessDeserializer::new(a))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<T, A::Error> {
            while a.next_element::<IgnoredAny>()?.is_some() {}
            Ok(T::default())
        }
        fn visit_unit<E: de::Error>(self) -> Result<T, E> {
            Ok(T::default())
        }
        fn visit_none<E: de::Error>(self) -> Result<T, E> {
            Ok(T::default())
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<T, D2::Error> {
            de_map_or_default(d)
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<T, E> {
            Ok(T::default())
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<T, E> {
            Ok(T::default())
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<T, E> {
            Ok(T::default())
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<T, E> {
            Ok(T::default())
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<T, E> {
            Ok(T::default())
        }
    }
    d.deserialize_any(V(PhantomData))
}

// ------------------------------------------------------------------ ints

/// `_int(v) = v if isinstance(v, int) else None`, plus an i64 range clamp.
/// bool IS int in Python, so `true` -> 1. Out-of-i64 -> NULL on both sides
/// (see the Python patch: v0 crashed SQLite instead).
#[derive(Debug, Clone, Copy, Default)]
pub struct IntOrNull(pub Option<i64>);

impl<'de> Deserialize<'de> for IntOrNull {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = IntOrNull;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(IntOrNull(Some(v as i64)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(IntOrNull(Some(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(IntOrNull(i64::try_from(v).ok()))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(IntOrNull(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(IntOrNull(None))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(IntOrNull(None))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(IntOrNull(None))
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                IntOrNull::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(IntOrNull(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(IntOrNull(None))
            }
        }
        d.deserialize_any(V)
    }
}

// ------------------------------------------------------------- timestamps

#[derive(Debug, Clone, Default)]
pub enum TsVal<'a> {
    #[default]
    Absent,
    Int(i128),
    Float(f64),
    Str(Cow<'a, str>),
    Other,
}

impl<'de: 'a, 'a> Deserialize<'de> for TsVal<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = TsVal<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            // Python: isinstance(True, int) -> parse_ts(True) == 1000.
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(TsVal::Int(v as i128))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(TsVal::Int(v as i128))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(TsVal::Int(v as i128))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(TsVal::Float(v))
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(TsVal::Str(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(TsVal::Str(Cow::Owned(v.to_owned())))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(TsVal::Other)
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(TsVal::Other)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                TsVal::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(TsVal::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(TsVal::Other)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// Days from civil, Howard Hinnant. Mirrors extract.py exactly.
#[inline]
fn days_from_civil(mut y: i64, mo: i64, d: i64) -> i64 {
    y -= (mo <= 2) as i64;
    let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `_TS_RE` = ^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?
/// ASCII digits only. v0's `\d` also matched Unicode decimal digits; that is
/// patched to `[0-9]` so this side is exact.
fn parse_ts_str(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    #[inline]
    fn num(b: &[u8], at: usize, n: usize) -> Option<i64> {
        let mut v: i64 = 0;
        for i in 0..n {
            let c = *b.get(at + i)?;
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    }
    let y = num(b, 0, 4)?;
    if b[4] != b'-' {
        return None;
    }
    let mo = num(b, 5, 2)?;
    if b[7] != b'-' {
        return None;
    }
    let d = num(b, 8, 2)?;
    if b[10] != b'T' && b[10] != b' ' {
        return None;
    }
    let h = num(b, 11, 2)?;
    if b[13] != b':' {
        return None;
    }
    let mi = num(b, 14, 2)?;
    if b[16] != b':' {
        return None;
    }
    let sec = num(b, 17, 2)?;
    let mut ms = (((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60 + sec) * 1000;
    if b.len() > 19 && b[19] == b'.' {
        let mut frac: Vec<u8> = Vec::with_capacity(3);
        let mut i = 20;
        while i < b.len() && b[i].is_ascii_digit() {
            if frac.len() < 3 {
                frac.push(b[i]);
            }
            i += 1;
        }
        if !frac.is_empty() {
            while frac.len() < 3 {
                frac.push(b'0');
            }
            let f = (frac[0] - b'0') as i64 * 100
                + (frac[1] - b'0') as i64 * 10
                + (frac[2] - b'0') as i64;
            ms += f;
        }
    }
    Some(ms)
}

/// Python: int/float -> int(v*1000) if v < 1e12 else int(v). Out-of-i64 -> NULL.
fn parse_ts(v: &TsVal) -> Option<i64> {
    match v {
        TsVal::Int(i) => {
            let r: i128 = if (*i as f64) < 1e12 { i * 1000 } else { *i };
            i64::try_from(r).ok()
        }
        TsVal::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                return None; // unreachable: the grammar rejects these literals
            }
            let r = if *f < 1e12 { *f * 1000.0 } else { *f };
            let t = r.trunc();
            if t >= -9.223372036854776e18 && t < 9.223372036854776e18 {
                Some(t as i64)
            } else {
                None
            }
        }
        TsVal::Str(s) => parse_ts_str(s),
        _ => None,
    }
}

// ---------------------------------------------------------------- content

#[derive(Debug, Clone, Default)]
pub enum ContentField<'a> {
    #[default]
    Absent,
    Str(Cow<'a, str>),
    List(Vec<Node<'a>>),
    Other,
}

#[derive(Debug, Clone)]
pub enum Node<'a> {
    Str(Cow<'a, str>),
    Block(Box<Block<'a>>),
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct Block<'a> {
    pub text: MaybeStr<'a>,
    pub content: ContentField<'a>,
    pub btype: MaybeStr<'a>,
    pub name: MaybeStr<'a>,
    pub input: InputMap,
    pub is_error: Present,
}
obj_de!(lenient Block<'a>, [
    "text" => text: MaybeStr<'a>,
    "content" => content: ContentField<'a>,
    "type" => btype: MaybeStr<'a>,
    "name" => name: MaybeStr<'a>,
    "input" => input: InputMap,
    "is_error" => is_error: Present,
]);

/// `input` as an ordered map. `preserve_order` makes serde_json's Map an
/// IndexMap, so a duplicate key overwrites IN PLACE -- exactly CPython dict.
/// A hand-rolled visitor that pushed every entry emitted BOTH values, adding
/// searchable text for a command the record does not contain.
#[derive(Debug, Clone, Default)]
pub struct InputMap(pub Option<Map<String, Value>>);

impl<'de> Deserialize<'de> for InputMap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(InputMap(Some(de_map_or_default::<D, Map<String, Value>>(d)?)))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for ContentField<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = ContentField<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(ContentField::Str(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ContentField::Str(Cow::Owned(v.to_owned())))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(n) = a.next_element::<Node<'a>>()? {
                    out.push(n);
                }
                Ok(ContentField::List(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(ContentField::Other)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(ContentField::Other)
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(ContentField::Other)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                ContentField::deserialize(d)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(ContentField::Other)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(ContentField::Other)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(ContentField::Other)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(ContentField::Other)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Node<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = Node<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(Node::Str(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Node::Str(Cow::Owned(v.to_owned())))
            }
            fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
                let b = Block::deserialize(de::value::MapAccessDeserializer::new(a))?;
                Ok(Node::Block(Box::new(b)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Node::Other)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                Node::deserialize(d)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

// ----------------------------------------------------------------- record

#[derive(Debug, Clone, Default)]
pub struct Msg<'a> {
    pub role: MaybeStr<'a>,
    pub model: MaybeStr<'a>,
    pub content: ContentField<'a>,
    pub usage: Usage,
}
obj_de!(lenient Msg<'a>, [
    "role" => role: MaybeStr<'a>,
    "model" => model: MaybeStr<'a>,
    "content" => content: ContentField<'a>,
    "usage" => usage: Usage,
]);

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: IntOrNull,
    pub output_tokens: IntOrNull,
    pub cache_read_input_tokens: IntOrNull,
}
obj_de!(lenient Usage, [
    "input_tokens" => input_tokens: IntOrNull,
    "output_tokens" => output_tokens: IntOrNull,
    "cache_read_input_tokens" => cache_read_input_tokens: IntOrNull,
]);

/// Top-level record. STRICT: only a JSON object becomes a Rec, so `[1,2,3]`
/// and `42` are `not_an_object` rejects. `#[derive]` would have happily built
/// this from a 3-element array by field position.
///
/// NOTE: no `#[serde(flatten)] _rest: IgnoredAny`. serde already ignores
/// unknown keys, and `flatten` forces the whole map to be BUFFERED -- the
/// very allocation such a field is supposed to avoid.
#[derive(Debug, Clone, Default)]
pub struct Rec<'a> {
    pub timestamp: TsVal<'a>,
    pub rtype: MaybeStr<'a>,
    pub message: Msg<'a>,
}
obj_de!(strict Rec<'a>, [
    "timestamp" => timestamp: TsVal<'a>,
    "type" => rtype: MaybeStr<'a>,
    "message" => message: Msg<'a>,
]);

// ------------------------------------------------------------ extraction

fn text_of(c: &ContentField, out: &mut Vec<String>) {
    match c {
        ContentField::Str(s) => {
            if !s.is_empty() {
                out.push(s.to_string());
            }
        }
        ContentField::List(nodes) => {
            for n in nodes {
                match n {
                    Node::Str(s) => {
                        if !s.is_empty() {
                            out.push(s.to_string());
                        }
                    }
                    Node::Other => {}
                    Node::Block(b) => {
                        if let Some(t) = b.text.as_str() {
                            if !t.is_empty() {
                                out.push(t.to_string());
                            }
                            continue; // Python `continue`s on a str `text`
                        }
                        match &b.content {
                            ContentField::Str(s) => {
                                if !s.is_empty() {
                                    out.push(s.to_string());
                                }
                            }
                            ContentField::List(_) => {
                                // Python appends the JOINED sub-result as ONE item.
                                let mut inner = Vec::new();
                                text_of(&b.content, &mut inner);
                                let j = inner.join(" ");
                                if !j.is_empty() {
                                    out.push(j);
                                }
                            }
                            _ => {
                                if b.btype.as_str() == Some("tool_use") {
                                    if let InputMap(Some(m)) = &b.input {
                                        for v in m.values() {
                                            if let Value::String(s) = v {
                                                if !s.is_empty() {
                                                    out.push(s.clone());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn flat_text(c: &ContentField) -> String {
    let mut v = Vec::new();
    text_of(c, &mut v);
    v.join(" ")
}

const TARGET_KEYS: [&str; 6] = [
    "file_path",
    "path",
    "notebook_path",
    "filePath",
    "url",
    "pattern",
];

/// Python truthiness of an arbitrary JSON value.
#[inline]
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `_tool_of`. Returns Err(reason) for a shape where Python would store a
/// NON-STRING in the TEXT column `tool` (SQLite would coerce it via TEXT
/// affinity, e.g. 42 -> '42'). Rather than reproduce that coercion, both sides
/// reject the record loudly. Corpus incidence measured at 0.
pub fn tool_of(
    c: &ContentField,
) -> Result<(Option<String>, Option<String>, Option<i64>), &'static str> {
    let mut tool: Option<String> = None;
    let mut tool_set = false; // mirrors Python's `tool is not None`
    let mut target: Option<String> = None;
    let mut is_err: Option<i64> = None;
    let nodes = match c {
        ContentField::List(n) => n,
        _ => return Ok((None, None, None)),
    };
    for n in nodes {
        let b = match n {
            Node::Block(b) => b,
            _ => continue,
        };
        if b.btype.as_str() == Some("tool_use") && !tool_set {
            match &b.name {
                MaybeStr::Str(s) => {
                    tool = Some(s.to_string());
                    tool_set = true;
                }
                // Python: `b.get("name")` is non-None, so `tool is None` is
                // False for every later block, AND a non-str lands in a TEXT
                // column. Refuse instead of guessing SQLite's coercion.
                MaybeStr::NotStr => return Err("tool_name_not_a_string"),
                // Absent -> Python leaves tool None, so a LATER tool_use wins.
                MaybeStr::Absent => {}
            }
            if let InputMap(Some(m)) = &b.input {
                let mut hit = false;
                for k in TARGET_KEYS {
                    if let Some(Value::String(s)) = m.get(k) {
                        if !s.is_empty() {
                            target = Some(s.clone());
                            hit = true;
                            break;
                        }
                    }
                }
                if !hit && target.is_none() {
                    if let Some(Value::String(cmd)) = m.get("command") {
                        if !cmd.is_empty() {
                            let end = cmd
                                .char_indices()
                                .nth(CMD_CHARS)
                                .map(|(i, _)| i)
                                .unwrap_or(cmd.len());
                            target = Some(cmd[..end].to_string());
                        }
                    }
                }
            }
        }
        match &b.is_error {
            Present::Val(v) => is_err = Some(py_truthy(v) as i64),
            Present::Absent => {
                if b.btype.as_str() == Some("tool_result") && is_err.is_none() {
                    is_err = Some(0);
                }
            }
        }
    }
    Ok((tool, target, is_err))
}

/// One extracted row.
#[derive(Debug, Default)]
pub struct Row {
    pub off: i64,
    pub len: i64,
    pub hash: i64,
    pub ts: Option<i64>,
    pub kind: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub tool: Option<String>,
    pub target: Option<String>,
    pub in_tok: Option<i64>,
    pub out_tok: Option<i64>,
    pub cache_r: Option<i64>,
    pub is_err: Option<i64>,
    pub text: String,
}

pub fn claude_code(r: &Rec, off: i64, len: i64, hash: i64) -> Result<Row, &'static str> {
    let (tool, target, is_err) = tool_of(&r.message.content)?;
    let u = &r.message.usage;
    Ok(Row {
        off,
        len,
        hash,
        ts: parse_ts(&r.timestamp),
        kind: r.rtype.as_str().map(|s| s.to_string()),
        role: r.message.role.as_str().map(|s| s.to_string()),
        model: r.message.model.as_str().map(|s| s.to_string()),
        tool,
        target,
        in_tok: u.input_tokens.0,
        out_tok: u.output_tokens.0,
        cache_r: u.cache_read_input_tokens.0,
        is_err,
        text: flat_text(&r.message.content),
    })
}

// -------------------------------------------------------------- scanner

pub struct ScanOut {
    pub rows: Vec<Row>,
    pub rejects: Vec<(i64, i64, &'static str)>,
    pub end: i64,
}

/// Framing identical to index.py `scan()`: a trailing line without \n is not
/// consumed, blank lines advance the offset only.
pub fn scan_buf(buf: &[u8], start: i64) -> ScanOut {
    let mut rows = Vec::new();
    let mut rejects: Vec<(i64, i64, &'static str)> = Vec::new();
    let mut off = start;
    let mut pos = 0usize;
    while pos < buf.len() {
        let nl = match memchr::memchr(b'\n', &buf[pos..]) {
            Some(i) => pos + i,
            None => break, // partial trailing line: cursor stops before it
        };
        let line = &buf[pos..=nl];
        let n = line.len() as i64;
        let s = strip_py(line);
        if !s.is_empty() {
            if n as usize > MAX_LINE {
                rejects.push((off, n, "record_too_large"));
            } else if bracket_count(s) > MAX_DEPTH && max_depth(s) > MAX_DEPTH {
                // Same two-step bound as the Python side: a memchr-class count
                // first, the exact scan only when it can possibly exceed.
                rejects.push((off, n, "depth_limit"));
            } else {
                match std::str::from_utf8(s) {
                    Err(_) => rejects.push((off, n, "invalid_utf8")),
                    Ok(txt) => match serde_json::from_str::<Rec>(txt) {
                        Err(_) => {
                            // Distinguish "not an object" the way Python does.
                            match serde_json::from_str::<serde_json::Value>(txt) {
                                Ok(Value::Object(_)) => rejects.push((off, n, "parse_error")),
                                Ok(_) => rejects.push((off, n, "not_an_object")),
                                Err(_) => rejects.push((off, n, "parse_error")),
                            }
                        }
                        Ok(rec) => match claude_code(&rec, off, n, rec_hash(s)) {
                            Ok(row) => rows.push(row),
                            Err(why) => rejects.push((off, n, why)),
                        },
                    },
                }
            }
        }
        off += n;
        pos = nl + 1;
    }
    ScanOut {
        rows,
        rejects,
        end: off,
    }
}
