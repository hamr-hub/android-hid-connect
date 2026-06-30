//! `k=v` token parser + writer + head-splitting helper.
//!
//! Every wire verb takes a variable number of arguments as
//! `key=value` tokens separated by ASCII whitespace. Values can be
//! quoted with `"…"` to embed whitespace or `=` inside them. This
//! module is the canonical parser used by both the daemon and the
//! client libraries.

use std::borrow::Cow;

/// Parse `k=v` tokens out of `input`, splitting on ASCII whitespace and
/// honouring `"…"` quoted values.
///
/// Tokens without an `=` sign are returned with an empty value (`("", token)`)
/// so callers can distinguish bare tokens (rare) from `k=` (key with
/// empty value).
///
/// Returns `Cow<str>` because tokens like `key="hello world"` need to
/// be unquoted before the caller can split on `=`, which requires
/// allocating a short intermediate when the value was quoted.
///
/// ## Recognised shapes
///
/// - `key=value`             — `value` runs until the next whitespace.
/// - `key="quoted value"`    — `value` may contain whitespace or `=`.
/// - `key=`                  — empty value, key preserved.
/// - `key`                   — bare token, treated as `("", key)`.
/// - `=value`                — empty key; returned as `("", "value")`.
///
/// Examples:
///
/// ```
/// use android_hid_protocol::parse_kv;
///
/// let collected: Vec<_> = parse_kv(r#"tap x=540 y=1200 text="hello world""#).collect();
/// assert_eq!(collected.len(), 4);
/// assert_eq!(collected[3].0, "text");
/// assert_eq!(collected[3].1, "hello world");
/// ```
pub fn parse_kv(input: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, str>)> {
    TokenIter { rest: input }
}

struct TokenIter<'a> {
    rest: &'a str,
}

impl<'a> Iterator for TokenIter<'a> {
    type Item = (Cow<'a, str>, Cow<'a, str>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let trimmed = self.rest.trim_start();
            if trimmed.is_empty() {
                self.rest = "";
                return None;
            }
            let (token, rest) = split_one_token(trimmed);
            self.rest = rest;
            if token.is_empty() {
                continue;
            }
            return Some(split_kv(token));
        }
    }
}

/// Pull off one whitespace-delimited token, respecting a `"…"` quoted
/// value attached via `key="…"`. Returns the **raw** token slice
/// (outer quotes included if any) plus the unconsumed remainder.
fn split_one_token(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            break;
        }
        if b == b'=' && bytes.get(i + 1) == Some(&b'"') {
            // Find the closing quote so embedded whitespace is not split.
            let mut j = i + 2;
            while j < n && bytes[j] != b'"' {
                j += 1;
            }
            i = if j < n { j + 1 } else { j };
            break;
        }
        i += 1;
    }
    let mut end = i;
    while end < n && bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    (&s[..i], &s[end..])
}

/// Split a single token into `(key, value)`, stripping the outer
/// `"…"` from a quoted value (or from a fully-quoted bare token).
fn split_kv<'a>(token: &'a str) -> (Cow<'a, str>, Cow<'a, str>) {
    let bytes = token.as_bytes();
    // Bare quoted token: `"value"` — no `=`.
    if token.find('=').is_none() {
        if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
            return (Cow::Borrowed(&token[1..bytes.len() - 1]), Cow::Borrowed(""));
        }
        return (Cow::Borrowed(token), Cow::Borrowed(""));
    }

    let eq_idx = token.find('=').expect("checked above");
    let key = &token[..eq_idx];
    let value_raw = &token[eq_idx + 1..];

    let vbytes = value_raw.as_bytes();
    if vbytes.len() >= 2 && vbytes[0] == b'"' && vbytes[vbytes.len() - 1] == b'"' {
        let inner = &value_raw[1..vbytes.len() - 1];
        return (Cow::Borrowed(key), Cow::Borrowed(inner));
    }

    (Cow::Borrowed(key), Cow::Borrowed(value_raw))
}

/// Split a command line into `(verb, rest)`.
///
/// `rest` is the **trimmed** remainder after the verb. If the input
/// has no argument tail, `rest` is `""`.
///
/// Examples:
///
/// ```
/// use android_hid_protocol::split_head;
///
/// assert_eq!(split_head("tap x=1 y=2"), ("tap", "x=1 y=2"));
/// assert_eq!(split_head("ping"), ("ping", ""));
/// assert_eq!(split_head("  tap   x=1  "), ("tap", "x=1"));
/// assert_eq!(split_head(""), ("", ""));
/// ```
pub fn split_head(cmd: &str) -> (&str, &str) {
    let trimmed = cmd.trim_start();
    let end = trimmed
        .as_bytes()
        .iter()
        .position(|b| b.is_ascii_whitespace())
        .unwrap_or(trimmed.len());
    let verb = &trimmed[..end];
    let rest = trimmed[end..].trim();
    (verb, rest)
}

/// Quote a value for the wire if it contains whitespace, `=`, or `"`.
///
/// - Returns `Cow::Borrowed` when the value needs no quoting.
/// - Returns `Cow::Owned` with `"…"` wrapping (and embedded `"` escaped
///   as `\"`) when it does.
pub fn quote_value(s: &str) -> Cow<'_, str> {
    if !needs_quoting(s) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('\\');
            out.push('"');
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    Cow::Owned(out)
}

fn needs_quoting(s: &str) -> bool {
    s.bytes()
        .any(|b| b.is_ascii_whitespace() || b == b'=' || b == b'"')
}

/// Incremental `k=v` writer.
///
/// Appends `key=value` tokens (whitespace-separated) to a backing
/// `String`. The first [`KvPair::value`] call inserts a leading space
/// if the buffer is non-empty, so the writer is safe to use for both
/// fresh buffers and existing tails.
///
/// Quotes the value automatically when it contains whitespace, `=`,
/// or `"` — matching the [`parse_kv`] reader.
///
/// `Debug` is hand-rolled because the struct holds a mutable reference
/// and `derive(Debug)` would require it. The output omits the
/// reference and only prints the internal `needs_sep` flag.
///
/// ### Example
///
/// ```text
/// let mut s = String::new();
/// let mut w = KvWriter::new(&mut s);
/// w.key("x").value("540");
/// w.key("text").value("hello world");
/// assert_eq!(s, r#"x=540 text="hello world""#);
/// ```
pub struct KvWriter<'a> {
    out: &'a mut String,
    needs_sep: bool,
}

impl std::fmt::Debug for KvWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvWriter")
            .field("needs_sep", &self.needs_sep)
            .finish()
    }
}

/// Single-key writer handed back by [`KvWriter::key`].
///
/// Consumed by [`KvPair::value`] to commit the pair.
pub struct KvPair<'a> {
    out: &'a mut String,
    needs_sep: &'a mut bool,
    key: String,
}

impl std::fmt::Debug for KvPair<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvPair")
            .field("key", &self.key)
            .finish()
    }
}

impl<'a> KvWriter<'a> {
    /// Bind the writer to a backing `String`.
    pub fn new(out: &'a mut String) -> Self {
        Self {
            out,
            needs_sep: false,
        }
    }

    /// Start a `key=value` pair. Returns a [`KvPair`] on which the
    /// caller invokes `.value(...)` once to commit.
    pub fn key(&mut self, k: &str) -> KvPair<'_> {
        KvPair {
            out: self.out,
            needs_sep: &mut self.needs_sep,
            key: k.to_owned(),
        }
    }
}

impl<'a> KvPair<'a> {
    /// Commit this pair with the given value. Quotes the value if it
    /// contains whitespace, `=`, or `"`.
    pub fn value(self, v: impl AsRef<str>) {
        let v = v.as_ref();
        if *self.needs_sep {
            self.out.push(' ');
        }
        self.out.push_str(&self.key);
        self.out.push('=');
        let quoted = quote_value(v);
        match quoted {
            Cow::Borrowed(b) => self.out.push_str(b),
            Cow::Owned(o) => self.out.push_str(&o),
        }
        *self.needs_sep = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_basic() {
        let v: Vec<(String, String)> = parse_kv("tap x=540 y=1200")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            v,
            [
                ("tap".to_owned(), "".to_owned()),
                ("x".to_owned(), "540".to_owned()),
                ("y".to_owned(), "1200".to_owned()),
            ]
        );
    }

    #[test]
    fn parser_quoted() {
        let v: Vec<(String, String)> = parse_kv(r#"text="hello world" x=1"#)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            v,
            [
                ("text".to_owned(), "hello world".to_owned()),
                ("x".to_owned(), "1".to_owned()),
            ]
        );
    }

    #[test]
    fn parser_quoted_with_equals() {
        let v: Vec<(String, String)> = parse_kv(r#"value="a=b" x=1"#)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            v,
            [
                ("value".to_owned(), "a=b".to_owned()),
                ("x".to_owned(), "1".to_owned()),
            ]
        );
    }

    #[test]
    fn parser_empty_value() {
        let v: Vec<(String, String)> = parse_kv("k=")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(v, [("k".to_owned(), "".to_owned())]);
    }

    #[test]
    fn parser_empty_key() {
        let v: Vec<(String, String)> = parse_kv("=value")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(v, [("".to_owned(), "value".to_owned())]);
    }

    #[test]
    fn parser_empty_input() {
        let v: Vec<(String, String)> = parse_kv("   \t  ")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(v.is_empty());
    }

    #[test]
    fn parser_multiple_spaces() {
        let v: Vec<(String, String)> = parse_kv("tap   x=540    y=1200")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            v,
            [
                ("tap".to_owned(), "".to_owned()),
                ("x".to_owned(), "540".to_owned()),
                ("y".to_owned(), "1200".to_owned()),
            ]
        );
    }

    #[test]
    fn parser_kv_then_bare_token() {
        let v: Vec<(String, String)> = parse_kv("k=1 bare")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            v,
            [
                ("k".to_owned(), "1".to_owned()),
                ("bare".to_owned(), "".to_owned()),
            ]
        );
    }

    #[test]
    fn quote_plain_is_borrowed() {
        let q = quote_value("hello");
        assert!(matches!(q, Cow::Borrowed(_)));
        assert_eq!(q, "hello");
    }

    #[test]
    fn quote_with_whitespace() {
        let q = quote_value("hello world");
        assert_eq!(q, "\"hello world\"");
    }

    #[test]
    fn quote_with_equals() {
        let q = quote_value("a=b");
        assert_eq!(q, "\"a=b\"");
    }

    #[test]
    fn quote_with_inner_quote() {
        let q = quote_value(r#"he said "hi""#);
        assert_eq!(q, r#""he said \"hi\"""#);
    }

    #[test]
    fn split_head_basic() {
        assert_eq!(split_head("tap x=540 y=1200"), ("tap", "x=540 y=1200"));
    }

    #[test]
    fn split_head_no_args() {
        assert_eq!(split_head("ping"), ("ping", ""));
    }

    #[test]
    fn split_head_empty_tail() {
        assert_eq!(split_head("quit   "), ("quit", ""));
    }

    #[test]
    fn split_head_leading_whitespace() {
        assert_eq!(split_head("   tap  x=1  "), ("tap", "x=1"));
    }

    #[test]
    fn split_head_empty_input() {
        assert_eq!(split_head(""), ("", ""));
        assert_eq!(split_head("   "), ("", ""));
    }

    #[test]
    fn kvwriter_basic() {
        let mut s = String::new();
        let mut w = KvWriter::new(&mut s);
        w.key("x").value("540");
        w.key("y").value("1200");
        assert_eq!(s, "x=540 y=1200");
    }

    #[test]
    fn kvwriter_quotes_automatically() {
        let mut s = String::new();
        let mut w = KvWriter::new(&mut s);
        w.key("text").value("hello world");
        w.key("flag").value("a=b");
        assert_eq!(s, r#"text="hello world" flag="a=b""#);
    }

    #[test]
    fn kvwriter_appends_to_existing() {
        let mut s = "prefix ".to_owned();
        let mut w = KvWriter::new(&mut s);
        w.key("k").value("v");
        assert_eq!(s, "prefix k=v");
    }

    #[test]
    fn kvwriter_empty_value() {
        let mut s = String::new();
        KvWriter::new(&mut s).key("k").value("");
        assert_eq!(s, "k=");
    }

    #[test]
    fn kvwriter_round_trip_with_parser() {
        let mut s = String::new();
        let mut w = KvWriter::new(&mut s);
        w.key("x").value("540");
        w.key("y").value("1200");
        w.key("text").value("hello world");
        w.key("eq").value("a=b");

        let parsed: Vec<(String, String)> = parse_kv(&s)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            parsed,
            [
                ("x".to_owned(), "540".to_owned()),
                ("y".to_owned(), "1200".to_owned()),
                ("text".to_owned(), "hello world".to_owned()),
                ("eq".to_owned(), "a=b".to_owned()),
            ]
        );
    }
}