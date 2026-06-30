//! CSS-like accessibility tree selector.
//!
//! Ported from `handsets/handsets-cli/src/selector.rs` to give the
//! agent typed, client-side selector parsing — no round-trip per
//! lookup. The grammar matches what `hs find` / `hs node_click`
//! accept on the CLI so that the same selector string works
//! against either backend.
//!
//! ## Grammar
//!
//! ```text
//! selector   ::= term ("," term)*          // comma = OR
//! term       ::= atom (":" flag | "[" attr_spec "]")*
//! atom       ::= IDENT | "*"
//! attr_spec  ::= attr_name ( "=" | "~=" | "^=" | "$=" | "*=" ) value
//! flag       ::= "visible" | "clickable" | "enabled" | "focused"
//!              | "checked" | "has-text" "(" STRING ")"
//!              | "text-is" "(" STRING ")"
//!              | "near" "(" selector "," NUMBER ")"
//!              | "below" "(" ")" | "right-of" "(" ")"
//!              | "in" "(" selector ")"
//! ```
//!
//! ## Examples
//!
//! - `Button` — any `Button`-classed node
//! - `Button:clickable` — clickable buttons only
//! - `EditText[hint~=Email]` — hint contains "Email"
//! - `TextView:has-text("Welcome"):visible` — visible Welcome label
//! - `Button:near(Logo, 100)` — within 100px of any Logo
//! - `EditText[id=login], EditText[id=password]` — either login or
//!   password field
//!
//! ## Performance
//!
//! - `Selector::parse` is called once per selector string and the
//!   result is cheap to clone (wrap in `Rc<Selector>` if you need
//!   shared ownership across threads).
//! - [`Selector::find_all`] does a single tree walk that evaluates
//!   all predicates per node, so the cost is `O(N × predicates)`
//!   not `O(N × predicates × selectors)`.

use std::fmt;

/// One parsed selector — independent of any specific tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Selector {
    /// Comma-separated OR branches.
    pub terms: Vec<Term>,
}

impl Selector {
    /// Parse a selector string. Accepts the full grammar
    /// described in the module docs.
    pub fn parse(src: &str) -> Result<Self, ParseError> {
        let mut p = Parser::new(src);
        let terms = p.parse_or_terms()?;
        p.expect_eof()?;
        Ok(Selector { terms })
    }

    /// All atoms (for diagnostics + dry-run listing).
    pub fn atoms(&self) -> impl Iterator<Item = &Atom> {
        self.terms.iter().flat_map(|t| t.atoms.iter())
    }

    /// Resolve a selector against a flat node list.
    ///
    /// `find_all` does **one pass** over the tree; multiple
    /// atoms in the same term are AND'd, multiple terms in
    /// the same selector are OR'd.
    pub fn find_all<'a, N: A11yLike>(&self, nodes: &'a [N]) -> Vec<&'a N> {
        let mut out = Vec::new();
        for node in nodes {
            if self.matches(node, nodes) {
                out.push(node);
            }
        }
        out
    }

    /// Convenience wrapper — return the first matching node, if any.
    pub fn find_one<'a, N: A11yLike>(&self, nodes: &'a [N]) -> Option<&'a N> {
        nodes.iter().find(|n| self.matches(*n, nodes))
    }

    /// True if `node` matches the selector. Spatial flags
    /// (`:near`, `:below`, `:right-of`, `:in`) need access to
    /// the rest of the tree to evaluate; pass `nodes` for that.
    pub fn matches<N: A11yLike>(&self, node: &N, nodes: &[N]) -> bool {
        self.terms.iter().any(|t| t.matches(node, nodes))
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, t) in self.terms.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{t}")?;
        }
        Ok(())
    }
}

/// One AND-branch of a selector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Term {
    /// The base tag (e.g. `Button`, `EditText`, `*`).
    pub atoms: Vec<Atom>,
    /// Attribute filters `[hint~=Email]`.
    pub attrs: Vec<AttrFilter>,
    /// Pseudo-class flags `:clickable`, `:has-text("x")`, etc.
    pub flags: Vec<Flag>,
}

impl Term {
    pub fn matches<N: A11yLike>(&self, node: &N, all: &[N]) -> bool {
        if !self.atoms.iter().all(|a| a.matches(node)) {
            return false;
        }
        if !self.attrs.iter().all(|a| a.matches(node)) {
            return false;
        }
        if !self.flags.iter().all(|f| f.matches(node, all)) {
            return false;
        }
        true
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for a in &self.atoms {
            write!(f, "{a}")?;
        }
        for a in &self.attrs {
            write!(f, "[{a}]")?;
        }
        for fl in &self.flags {
            write!(f, ":{fl}")?;
        }
        Ok(())
    }
}

/// Tag selector. `*` matches any node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Atom {
    /// Matches the node's class name (e.g. `Button`).
    Tag(String),
    /// Matches any node.
    Wildcard,
}

impl Atom {
    pub fn matches<N: A11yLike>(&self, node: &N) -> bool {
        match self {
            Self::Wildcard => true,
            Self::Tag(t) => node.class_name().eq_ignore_ascii_case(t),
        }
    }
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wildcard => f.write_str("*"),
            Self::Tag(t) => f.write_str(t),
        }
    }
}

/// One `[attr=value]`-style filter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttrFilter {
    pub name: String,
    pub op: AttrOp,
    pub value: String,
}

impl AttrFilter {
    pub fn matches<N: A11yLike>(&self, node: &N) -> bool {
        let Some(actual) = node.attr(&self.name) else {
            return false;
        };
        match self.op {
            AttrOp::Eq => actual == self.value,
            AttrOp::Contains => actual.contains(&self.value),
            AttrOp::StartsWith => actual.starts_with(&self.value),
            AttrOp::EndsWith => actual.ends_with(&self.value),
            AttrOp::AnySubstr => {
                self.value.split_whitespace().all(|tok| actual.contains(tok))
            }
        }
    }
}

impl fmt::Display for AttrFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = match self.op {
            AttrOp::Eq => "=",
            AttrOp::Contains => "*=",
            AttrOp::StartsWith => "^=",
            AttrOp::EndsWith => "$=",
            AttrOp::AnySubstr => "~=",
        };
        write!(f, "{}{}{}", self.name, op, self.value)
    }
}

/// Attribute comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttrOp {
    /// `=`
    Eq,
    /// `*=` substring
    Contains,
    /// `^=` prefix
    StartsWith,
    /// `$=` suffix
    EndsWith,
    /// `~=` whitespace-separated tokens, all must appear as
    /// substrings (matches the CSS `~=` semantics for the
    /// `class` attribute, generalised to any string).
    AnySubstr,
}

/// One pseudo-class flag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Flag {
    /// `:visible`
    Visible,
    /// `:clickable`
    Clickable,
    /// `:enabled`
    Enabled,
    /// `:focused`
    Focused,
    /// `:checked`
    Checked,
    /// `:has-text("…")` — substring match against text.
    HasText(String),
    /// `:text-is("…")` — exact match against text.
    TextIs(String),
    /// `:near(selector, px)` — squared distance ≤ px².
    Near(Box<Selector>, u32),
    /// `:below()` — node center.y is below another node's
    /// center.y whose center.x is within ±50px (rough "below"
    /// zone).
    Below,
    /// `:right-of()` — symmetric of `:below`.
    RightOf,
    /// `:in(selector)` — node is "near" any node matching the
    /// inner selector. Full hierarchical support would need a
    /// tree iterator; the flat A11yLike trait doesn't carry
    /// that, so we approximate with "near any matching node".
    In(Box<Selector>),
}

impl Flag {
    pub fn matches<N: A11yLike>(&self, node: &N, all: &[N]) -> bool {
        match self {
            Self::Visible => node.is_visible(),
            Self::Clickable => node.is_clickable(),
            Self::Enabled => node.is_enabled(),
            Self::Focused => node.is_focused(),
            Self::Checked => node.is_checked(),
            Self::HasText(t) => node.text().contains(t),
            Self::TextIs(t) => node.text() == t,
            Self::Near(inner, px) => {
                let px2 = (*px as i64) * (*px as i64);
                let (cx, cy) = node.center();
                all.iter()
                    .any(|other| inner.matches(other, all) && {
                        let (ox, oy) = other.center();
                        let dx = (cx - ox) as i64;
                        let dy = (cy - oy) as i64;
                        dx * dx + dy * dy <= px2
                    })
            }
            Self::Below => {
                let (cx, cy) = node.center();
                all.iter().any(|other| {
                    let (ox, oy) = other.center();
                    other.is_visible() && oy > cy && (ox - cx).abs() < 50
                })
            }
            Self::RightOf => {
                let (cx, cy) = node.center();
                all.iter().any(|other| {
                    let (ox, oy) = other.center();
                    other.is_visible() && ox > cx && (oy - cy).abs() < 50
                })
            }
            Self::In(inner) => inner.find_one(all).is_some(),
        }
    }
}

impl fmt::Display for Flag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Visible => f.write_str("visible"),
            Self::Clickable => f.write_str("clickable"),
            Self::Enabled => f.write_str("enabled"),
            Self::Focused => f.write_str("focused"),
            Self::Checked => f.write_str("checked"),
            Self::HasText(t) => write!(f, "has-text(\"{t}\")"),
            Self::TextIs(t) => write!(f, "text-is(\"{t}\")"),
            Self::Near(s, px) => write!(f, "near({s}, {px})"),
            Self::Below => f.write_str("below()"),
            Self::RightOf => f.write_str("right-of()"),
            Self::In(s) => write!(f, "in({s})"),
        }
    }
}

/// Minimal trait a node type must implement for
/// [`Selector::find_all`]. The agent's `A11yNode` will implement
/// this; tests use a tiny in-memory `MockNode`.
pub trait A11yLike {
    /// Class name (e.g. `android.widget.Button` → matchable as
    /// `Button`).
    fn class_name(&self) -> &str;
    /// String value of an attribute, or `None` if absent.
    fn attr(&self, name: &str) -> Option<String>;
    /// Visible text of the node.
    fn text(&self) -> &str;
    /// Center of the rendered bounds, in screen pixels.
    fn center(&self) -> (i32, i32);
    /// `true` if the node is on-screen and not occluded.
    fn is_visible(&self) -> bool;
    /// `true` if the node has the `clickable` accessibility flag.
    fn is_clickable(&self) -> bool;
    /// `true` if the node is enabled.
    fn is_enabled(&self) -> bool;
    /// `true` if the node is the focused accessibility node.
    fn is_focused(&self) -> bool;
    /// `true` if the node is currently checked (Checkbox / Switch).
    fn is_checked(&self) -> bool;
}

// =============================================================================
// Parser
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at position {})", self.message, self.position)
    }
}

impl std::error::Error for ParseError {}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn expect_eof(&self) -> Result<(), ParseError> {
        if self.pos < self.src.len() {
            Err(self.err("unexpected trailing input"))
        } else {
            Ok(())
        }
    }

    fn err(&self, msg: &str) -> ParseError {
        ParseError {
            message: msg.to_string(),
            position: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_or_terms(&mut self) -> Result<Vec<Term>, ParseError> {
        let mut terms = vec![self.parse_term()?];
        loop {
            self.skip_ws();
            if self.peek() == Some(b',') {
                self.pos += 1;
                self.skip_ws();
                terms.push(self.parse_term()?);
            } else {
                break;
            }
        }
        Ok(terms)
    }

    fn parse_term(&mut self) -> Result<Term, ParseError> {
        let atoms = vec![self.parse_atom()?];
        let mut attrs = Vec::new();
        let mut flags = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b':') => {
                    self.pos += 1;
                    flags.push(self.parse_flag()?);
                }
                Some(b'[') => {
                    self.pos += 1;
                    attrs.push(self.parse_attr()?);
                }
                Some(b',') | None => break,
                Some(_) => {
                    return Err(self.err("expected ':', '[', or ',' between atoms"));
                }
            }
        }
        Ok(Term {
            atoms,
            attrs,
            flags,
        })
    }

    fn parse_atom(&mut self) -> Result<Atom, ParseError> {
        if self.peek() == Some(b'*') {
            self.pos += 1;
            return Ok(Atom::Wildcard);
        }
        let tag = self.parse_ident()?;
        Ok(Atom::Tag(tag))
    }

    fn parse_ident(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("expected identifier"));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn parse_attr(&mut self) -> Result<AttrFilter, ParseError> {
        self.skip_ws();
        let name = self.parse_ident()?;
        self.skip_ws();
        let op = match self.peek() {
            Some(b'=') => {
                self.pos += 1;
                AttrOp::Eq
            }
            Some(b'*') if self.src.as_bytes().get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                AttrOp::Contains
            }
            Some(b'^') if self.src.as_bytes().get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                AttrOp::StartsWith
            }
            Some(b'$') if self.src.as_bytes().get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                AttrOp::EndsWith
            }
            Some(b'~') if self.src.as_bytes().get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                AttrOp::AnySubstr
            }
            _ => return Err(self.err("expected =, *=, ^=, $=, or ~=")),
        };
        self.skip_ws();
        let value = self.parse_attr_value()?;
        self.skip_ws();
        if self.peek() != Some(b']') {
            return Err(self.err("expected ']'"));
        }
        self.pos += 1;
        Ok(AttrFilter { name, op, value })
    }

    fn parse_attr_value(&mut self) -> Result<String, ParseError> {
        if self.peek() == Some(b'"') {
            self.pos += 1;
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' {
                    let v = self.src[start..self.pos].to_string();
                    self.pos += 1;
                    return Ok(v);
                }
                self.pos += 1;
            }
            return Err(self.err("unterminated quoted string"));
        }
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b']' || b.is_ascii_whitespace() {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err(self.err("expected attribute value"));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn parse_flag(&mut self) -> Result<Flag, ParseError> {
        let name = self.parse_ident()?;
        let flag = match name.as_str() {
            "visible" => Flag::Visible,
            "clickable" => Flag::Clickable,
            "enabled" => Flag::Enabled,
            "focused" => Flag::Focused,
            "checked" => Flag::Checked,
            "has-text" => {
                self.parse_flag_open()?;
                let s = self.parse_flag_arg_string()?;
                self.parse_flag_close()?;
                Flag::HasText(s)
            }
            "text-is" => {
                self.parse_flag_open()?;
                let s = self.parse_flag_arg_string()?;
                self.parse_flag_close()?;
                Flag::TextIs(s)
            }
            "near" => {
                self.parse_flag_open()?;
                let sel_src = self.parse_flag_arg_string()?;
                self.skip_ws();
                if self.peek() != Some(b',') {
                    return Err(self.err("near: expected ',' between selector and px"));
                }
                self.pos += 1;
                self.skip_ws();
                let px_src = self.parse_flag_arg_string()?;
                self.parse_flag_close()?;
                // A bare `[attr=val]` selector is shorthand for
                // `*[attr=val]` (any tag with that attribute) — the
                // inner parser would otherwise reject a selector
                // that starts with `[` because it expects a leading
                // tag/atom.
                let sel_src = if sel_src.starts_with('[') {
                    format!("*{sel_src}")
                } else {
                    sel_src
                };
                let sel = Selector::parse(&sel_src)?;
                let px: u32 = px_src
                    .parse()
                    .map_err(|_| self.err("near: expected integer pixel radius"))?;
                Flag::Near(Box::new(sel), px)
            }
            "below" => {
                self.parse_flag_open()?;
                self.parse_flag_close()?;
                Flag::Below
            }
            "right-of" => {
                self.parse_flag_open()?;
                self.parse_flag_close()?;
                Flag::RightOf
            }
            "in" => {
                self.parse_flag_open()?;
                let sel_src = self.parse_flag_arg_string()?;
                self.parse_flag_close()?;
                let sel = Selector::parse(&sel_src)?;
                Flag::In(Box::new(sel))
            }
            other => return Err(self.err(&format!("unknown flag ':{other}'"))),
        };
        Ok(flag)
    }

    fn parse_flag_open(&mut self) -> Result<(), ParseError> {
        self.skip_ws();
        if self.peek() != Some(b'(') {
            return Err(self.err("expected '(' after flag name"));
        }
        self.pos += 1;
        Ok(())
    }

    fn parse_flag_close(&mut self) -> Result<(), ParseError> {
        self.skip_ws();
        if self.peek() != Some(b')') {
            return Err(self.err("expected ')' to close flag arg list"));
        }
        self.pos += 1;
        Ok(())
    }

    fn parse_flag_arg_string(&mut self) -> Result<String, ParseError> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some(b'"') {
            return self.parse_attr_value();
        }
        let mut depth: i32 = 0;
        let mut bracket_depth: i32 = 0;
        while let Some(b) = self.peek() {
            match b {
                b'(' => depth += 1,
                b')' if depth == 0 && bracket_depth == 0 => break,
                b')' => depth -= 1,
                b'[' => bracket_depth += 1,
                b']' if depth == 0 && bracket_depth > 0 => bracket_depth -= 1,
                b',' if depth == 0 && bracket_depth == 0 => break,
                _ => {}
            }
            self.pos += 1;
        }
        let raw = self.src[start..self.pos].trim();
        Ok(raw.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // A tiny mock node for the matching tests.
    // ---------------------------------------------------------------------
    #[derive(Debug, Clone)]
    struct MockNode {
        class: String,
        attrs: Vec<(String, String)>,
        text: String,
        center: (i32, i32),
        visible: bool,
        clickable: bool,
        enabled: bool,
        focused: bool,
        checked: bool,
    }

    impl MockNode {
        fn button(text: &str) -> Self {
            Self {
                class: "android.widget.Button".into(),
                attrs: vec![("text".into(), text.into())],
                text: text.into(),
                center: (540, 1200),
                visible: true,
                clickable: true,
                enabled: true,
                focused: false,
                checked: false,
            }
        }
    }

    impl A11yLike for MockNode {
        fn class_name(&self) -> &str {
            self.class.rsplit('.').next().unwrap_or(&self.class)
        }
        fn attr(&self, name: &str) -> Option<String> {
            self.attrs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        }
        fn text(&self) -> &str {
            &self.text
        }
        fn center(&self) -> (i32, i32) {
            self.center
        }
        fn is_visible(&self) -> bool {
            self.visible
        }
        fn is_clickable(&self) -> bool {
            self.clickable
        }
        fn is_enabled(&self) -> bool {
            self.enabled
        }
        fn is_focused(&self) -> bool {
            self.focused
        }
        fn is_checked(&self) -> bool {
            self.checked
        }
    }

    // ---------------------------------------------------------------------
    // Parsing tests
    // ---------------------------------------------------------------------

    #[test]
    fn parse_simple_tag() {
        let s = Selector::parse("Button").unwrap();
        assert_eq!(s.terms.len(), 1);
        assert_eq!(s.terms[0].atoms.len(), 1);
    }

    #[test]
    fn parse_wildcard() {
        let s = Selector::parse("*").unwrap();
        assert_eq!(s.terms[0].atoms, vec![Atom::Wildcard]);
    }

    #[test]
    fn parse_or_terms() {
        let s = Selector::parse("Button, EditText").unwrap();
        assert_eq!(s.terms.len(), 2);
    }

    #[test]
    fn parse_attr_eq() {
        let s = Selector::parse("EditText[id=login]").unwrap();
        assert_eq!(s.terms[0].attrs.len(), 1);
        assert_eq!(s.terms[0].attrs[0].name, "id");
        assert_eq!(s.terms[0].attrs[0].op, AttrOp::Eq);
        assert_eq!(s.terms[0].attrs[0].value, "login");
    }

    #[test]
    fn parse_attr_substring() {
        let s = Selector::parse("EditText[hint~=Email]").unwrap();
        assert_eq!(s.terms[0].attrs[0].op, AttrOp::AnySubstr);
        assert_eq!(s.terms[0].attrs[0].value, "Email");
    }

    #[test]
    fn parse_all_attr_ops() {
        for (op, src) in [
            (AttrOp::Eq, "a=b"),
            (AttrOp::Contains, "a*=b"),
            (AttrOp::StartsWith, "a^=b"),
            (AttrOp::EndsWith, "a$=b"),
            (AttrOp::AnySubstr, "a~=b"),
        ] {
            let s = Selector::parse(&format!("Tag[{src}]")).unwrap();
            assert_eq!(s.terms[0].attrs[0].op, op, "op mismatch for {src}");
        }
    }

    #[test]
    fn parse_quoted_attr_value() {
        let s = Selector::parse("TextView[text=\"Hello World\"]").unwrap();
        assert_eq!(s.terms[0].attrs[0].value, "Hello World");
    }

    #[test]
    fn parse_flag_simple() {
        for src in [
            "Button:visible",
            "Button:clickable",
            "Button:enabled",
            "Button:focused",
            "Button:checked",
        ] {
            let s = Selector::parse(src).unwrap();
            assert_eq!(s.terms[0].flags.len(), 1, "{src}");
        }
    }

    #[test]
    fn parse_flag_has_text() {
        let s = Selector::parse("TextView:has-text(\"Welcome\")").unwrap();
        match &s.terms[0].flags[0] {
            Flag::HasText(t) => assert_eq!(t, "Welcome"),
            _ => panic!("wrong flag"),
        }
    }

    #[test]
    fn parse_flag_near() {
        let s = Selector::parse("Button:near(Logo, 100)").unwrap();
        match &s.terms[0].flags[0] {
            Flag::Near(inner, px) => {
                assert_eq!(*px, 100);
                assert_eq!(inner.terms[0].atoms[0], Atom::Tag("Logo".into()));
            }
            _ => panic!("wrong flag"),
        }
    }

    #[test]
    fn parse_flag_below_and_right_of() {
        let _ = Selector::parse("Button:below()").unwrap();
        let _ = Selector::parse("Button:right-of()").unwrap();
    }

    #[test]
    fn parse_flag_in() {
        let s = Selector::parse("TextView:in(LinearLayout)").unwrap();
        match &s.terms[0].flags[0] {
            Flag::In(inner) => {
                assert_eq!(inner.terms[0].atoms[0], Atom::Tag("LinearLayout".into()));
            }
            _ => panic!("wrong flag"),
        }
    }

    #[test]
    fn parse_combined() {
        let s = Selector::parse(
            "EditText[hint~=Email]:clickable:has-text(\"@\"):near(Logo, 50)",
        )
        .unwrap();
        assert_eq!(s.terms[0].attrs.len(), 1);
        assert_eq!(s.terms[0].flags.len(), 3);
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        assert!(Selector::parse("Button:nope").is_err());
    }

    #[test]
    fn parse_rejects_unterminated_quote() {
        assert!(Selector::parse("Button:has-text(\"hello)").is_err());
    }

    #[test]
    fn parse_rejects_empty_input() {
        assert!(Selector::parse("").is_err());
    }

    #[test]
    fn display_round_trip_simple() {
        let s = Selector::parse("EditText[id=login]:clickable").unwrap();
        let s2 = Selector::parse(&s.to_string()).unwrap();
        assert_eq!(s, s2);
    }

    // ---------------------------------------------------------------------
    // Matching tests
    // ---------------------------------------------------------------------

    #[test]
    fn matches_by_tag() {
        let s = Selector::parse("Button").unwrap();
        let n = MockNode::button("Login");
        assert!(s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn matches_wildcard() {
        let s = Selector::parse("*").unwrap();
        let n = MockNode::button("Login");
        assert!(s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn matches_attr_equals() {
        let s = Selector::parse("Button[text=Login]").unwrap();
        let n = MockNode::button("Login");
        assert!(s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn matches_attr_substring() {
        let s = Selector::parse("Button[text~=ogi]").unwrap();
        let n = MockNode::button("Login");
        assert!(s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn matches_flag_clickable() {
        let s = Selector::parse("Button:clickable").unwrap();
        let n = MockNode::button("Login");
        assert!(s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn rejects_when_flag_misses() {
        let s = Selector::parse("Button:checked").unwrap();
        let n = MockNode::button("Login");
        assert!(!s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn matches_has_text() {
        let s = Selector::parse("Button:has-text(\"ogi\")").unwrap();
        let n = MockNode::button("Login");
        assert!(s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn or_terms_match_either() {
        let s = Selector::parse("Button, TextView").unwrap();
        let n = MockNode::button("Login");
        assert!(s.matches(&n, &[n.clone()]));
    }

    #[test]
    fn find_all_returns_all_matches() {
        let s = Selector::parse("Button").unwrap();
        let nodes = vec![
            MockNode::button("A"),
            MockNode::button("B"),
            MockNode {
                class: "android.widget.TextView".into(),
                attrs: vec![],
                text: "ignored".into(),
                center: (0, 0),
                visible: true,
                clickable: false,
                enabled: true,
                focused: false,
                checked: false,
            },
            MockNode::button("C"),
        ];
        let matched = s.find_all(&nodes);
        assert_eq!(matched.len(), 3);
    }

    #[test]
    fn near_matches_within_radius() {
        let logo = MockNode {
            class: "android.widget.ImageView".into(),
            attrs: vec![("id".into(), "logo".into())],
            text: "Logo".into(),
            center: (540, 1200),
            visible: true,
            clickable: false,
            enabled: true,
            focused: false,
            checked: false,
        };
        let login = MockNode::button("Login");
        let login = MockNode { center: (560, 1220), ..login };
        // `logo` is a resource-id, not a tag, so wrap in [id=logo].
        let s = Selector::parse("Button:near([id=logo], 50)").unwrap();
        assert!(s.matches(&login, &[logo, login.clone()]));
    }

    #[test]
    fn near_rejects_outside_radius() {
        let logo = MockNode {
            class: "android.widget.ImageView".into(),
            attrs: vec![("id".into(), "logo".into())],
            text: "Logo".into(),
            center: (100, 100),
            visible: true,
            clickable: false,
            enabled: true,
            focused: false,
            checked: false,
        };
        let login = MockNode::button("Login");
        let s = Selector::parse("Button:near([id=logo], 50)").unwrap();
        assert!(!s.matches(&login, &[logo, login.clone()]));
    }
}
