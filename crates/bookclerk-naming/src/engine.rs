//! Template parser + evaluator.
//!
//! Ports Libation's NamingTemplate engine (`FileManager.NamingTemplate`): tag /
//! conditional matching + tree evaluation. Also accepts older Libation forms
//! (`<if series>…<end if>`, `%asin%`, bare `<asin>`).

use std::sync::OnceLock;

use regex::Regex;

use crate::compare::{eval_is, eval_op, has_value};
use crate::context::{BookContext, ChapterContext};
use crate::tags::{self, canonical, eval_display, resolve_property};

/// A single evaluated fragment of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplatePart {
    /// The rendered text.
    pub value: String,
    /// `true` for literal template text, `false` for a resolved tag value.
    pub is_literal: bool,
}

#[derive(Debug, Clone)]
/// One node in the parsed naming-template AST.
enum Node {
    /// Verbatim template text copied into the rendered path.
    Literal(String),
    /// Book or chapter property tag, optionally with a format specifier.
    Property {
        /// Canonical tag name after alias resolution (`id`, `firstauthor`, …).
        canon: String,
        /// Optional `[format]` specifier from `<tag [format]>`.
        format: Option<String>,
    },
    /// Block rendered only when `cond` (optionally negated) is true.
    Conditional {
        /// When true, the `!` prefix inverted the condition.
        not: bool,
        /// Predicate evaluated against the book/chapter context.
        cond: Cond,
        /// Nested nodes rendered when the condition holds.
        children: Vec<Node>,
    },
}

#[derive(Debug, Clone)]
/// Predicate for a naming-template conditional block.
enum Cond {
    /// True when the book is in a series or is a podcast parent.
    IfSeries,
    /// True when the book is a podcast episode or podcast parent.
    IfPodcast,
    /// True only for a podcast-parent (show) title.
    IfPodcastParent,
    /// True for a book series that is not a podcast.
    IfBookseries,
    /// True when the edition is marked abridged.
    IfAbridged,
    /// True when the named property has a non-empty value.
    Has {
        /// Property name tested for a non-empty value; omitted uses the empty property.
        property: Option<String>,
    },
    /// True when the property matches an `[is]` check (empty, numeric, …).
    Is {
        /// Property name compared against the optional `[check]`.
        property: Option<String>,
        /// Optional `[check]` argument for `eval_is` (for example `empty`).
        check: Option<String>,
    },
    /// Compares two property values with a symbolic or `:named:` operator.
    Cmp {
        /// Left-hand property name.
        p1: String,
        /// Comparison operator token (`=`, `:contains:`, …).
        op: String,
        /// Right-hand property name.
        p2: String,
    },
}

impl Cond {
    /// Canonical close-tag name that matches this condition (`<-has>`, `<-ifseries>`).
    fn close_canon(&self) -> &'static str {
        match self {
            Cond::IfSeries => "ifseries",
            Cond::IfPodcast => "ifpodcast",
            Cond::IfPodcastParent => "ifpodcastparent",
            Cond::IfBookseries => "ifbookseries",
            Cond::IfAbridged => "ifabridged",
            Cond::Has { .. } => "has",
            Cond::Is { .. } => "is",
            Cond::Cmp { .. } => "cmp",
        }
    }
}

/// A parsed naming template (internal AST wrapper).
#[derive(Debug, Clone)]
pub(crate) struct Template {
    /// Top-level AST nodes of the parsed template.
    nodes: Vec<Node>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Open conditional (or root) while scanning the template left to right.
struct Frame {
    /// Open condition and its `!` flag; `None` on the root frame.
    cond: Option<(bool, Cond)>,
    /// Legacy `<if X>` frames close on `<end if>` rather than `<-name>`.
    legacy: bool,
    /// Nodes accumulated inside this open frame.
    children: Vec<Node>,
}

/// Parses a naming template into an AST, accepting Libation and legacy tag forms.
pub(crate) fn parse_template(template: &str) -> Template {
    let mut stack: Vec<Frame> = vec![Frame {
        cond: None,
        legacy: false,
        children: Vec::new(),
    }];
    let mut lit = String::new();
    let mut rest = template;

    macro_rules! flush_lit {
        () => {
            if !lit.is_empty() {
                stack
                    .last_mut()
                    .unwrap()
                    .children
                    .push(Node::Literal(std::mem::take(&mut lit)));
            }
        };
    }

    while !rest.is_empty() {
        if let Some((consumed, close_canon, legacy)) = match_close(rest) {
            flush_lit!();
            close_frame(&mut stack, &close_canon, legacy);
            rest = &rest[consumed..];
            continue;
        }
        if let Some((consumed, not, cond)) = match_open_conditional(rest) {
            flush_lit!();
            stack.push(Frame {
                cond: Some((not, cond)),
                legacy: false,
                children: Vec::new(),
            });
            rest = &rest[consumed..];
            continue;
        }
        if let Some((consumed, canon, format)) = match_property(rest) {
            flush_lit!();
            stack
                .last_mut()
                .unwrap()
                .children
                .push(Node::Property { canon, format });
            rest = &rest[consumed..];
            continue;
        }
        if let Some((consumed, cond)) = match_legacy_open(rest) {
            flush_lit!();
            stack.push(Frame {
                cond: Some((false, cond)),
                legacy: true,
                children: Vec::new(),
            });
            rest = &rest[consumed..];
            continue;
        }
        if let Some((consumed, canon, format)) = match_percent(rest) {
            flush_lit!();
            stack
                .last_mut()
                .unwrap()
                .children
                .push(Node::Property { canon, format });
            rest = &rest[consumed..];
            continue;
        }
        // Literal character.
        let ch = rest.chars().next().unwrap();
        lit.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush_lit!();

    // Unwind any unclosed frames (attach children conditionally to keep output).
    while stack.len() > 1 {
        let frame = stack.pop().unwrap();
        wrap_and_attach(stack.last_mut().unwrap(), frame);
    }

    Template {
        nodes: stack.pop().unwrap().children,
    }
}

/// Closes a frame into a [`Node::Conditional`] (or empty literal) and appends it to the parent.
fn wrap_and_attach(parent: &mut Frame, frame: Frame) {
    let node = match frame.cond {
        Some((not, cond)) => Node::Conditional {
            not,
            cond,
            children: frame.children,
        },
        None => Node::Literal(String::new()),
    };
    parent.children.push(node);
}

/// Unwinds to the matching open tag; unmatched closers are dropped.
fn close_frame(stack: &mut Vec<Frame>, close_canon: &str, legacy: bool) {
    // Find the nearest matching open frame.
    let mut idx = None;
    for (i, f) in stack.iter().enumerate().rev() {
        match &f.cond {
            Some((_, cond)) if legacy && f.legacy => {
                let _ = cond;
                idx = Some(i);
                break;
            }
            Some((_, cond)) if !legacy && !f.legacy && cond.close_canon() == close_canon => {
                idx = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(idx) = idx else {
        return; // no matching open tag; drop the closing tag
    };
    // Unwind down to (and including) idx.
    while stack.len() > idx + 1 {
        let frame = stack.pop().unwrap();
        wrap_and_attach(stack.last_mut().unwrap(), frame);
    }
    let frame = stack.pop().unwrap();
    wrap_and_attach(stack.last_mut().unwrap(), frame);
}

// ---------------------------------------------------------------------------
// Matchers
// ---------------------------------------------------------------------------

/// Turn a tag display name into a regex fragment (spaces -> `\s*`, `#` escaped).
fn tag_name_regex(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        match ch {
            ' ' => out.push_str(r"\s*"),
            '#' => out.push_str(r"\#"),
            c => out.push(c),
        }
    }
    out
}

/// Regex fragment for an optional `[format]` group (quoted, escaped, or bare).
const FORMAT_GROUP: &str = r#"(?:\\.|'[^']*'|"[^"]*"|[^'"\\\]])*"#;

/// Cached `<tag>` / `<tag [format]>` regexes, longest display names first so prefixes do not shadow.
fn property_matchers() -> &'static [(Regex, String)] {
    static M: OnceLock<Vec<(Regex, String)>> = OnceLock::new();
    M.get_or_init(|| {
        // (display, allows_format, canon)
        let mut entries: Vec<(String, bool, String)> = Vec::new();
        for (name, allows) in tags::property_tags() {
            entries.push(((*name).to_string(), *allows, canonical(name)));
        }
        // Legacy / alternate spellings.
        for (name, allows, canon) in [
            ("asin", false, "id"),
            ("author first", true, "firstauthor"),
            ("narrator first", true, "firstnarrator"),
            ("chapter title", true, "chtitle"),
            ("chapter #", true, "ch#"),
            ("length", true, "minutes"),
            ("full title", true, "title"),
            ("narrators", true, "narrator"),
            ("authors", true, "author"),
        ] {
            entries.push((name.to_string(), allows, canon.to_string()));
        }
        // Longest display names first so prefixes never shadow longer tags.
        entries.sort_by_key(|e| std::cmp::Reverse(e.0.len()));

        entries
            .into_iter()
            .map(|(name, allows, canon)| {
                let name_re = tag_name_regex(&name);
                let pattern = if allows {
                    format!(r"^<{name_re}(?:\s*\[(?P<format>{FORMAT_GROUP})\])?\s*>")
                } else {
                    format!(r"^<{name_re}>")
                };
                (Regex::new(&pattern).unwrap(), canon)
            })
            .collect()
    })
}

/// Consumes a leading `<property>` tag, returning bytes consumed, canonical name, and format.
fn match_property(rest: &str) -> Option<(usize, String, Option<String>)> {
    if !rest.starts_with('<') {
        return None;
    }
    for (re, canon) in property_matchers() {
        if let Some(caps) = re.captures(rest) {
            let whole = caps.get(0).unwrap();
            let format = caps.name("format").map(|m| m.as_str().to_string());
            return Some((whole.end(), canon.clone(), format));
        }
    }
    None
}

/// Cached `<if …->` regexes for boolean conditionals, longest names first.
fn bool_conditionals() -> &'static [(Regex, Cond)] {
    static M: OnceLock<Vec<(Regex, Cond)>> = OnceLock::new();
    M.get_or_init(|| {
        // Longest names first.
        let list = [
            ("if podcastparent", Cond::IfPodcastParent),
            ("if bookseries", Cond::IfBookseries),
            ("if podcast", Cond::IfPodcast),
            ("if series", Cond::IfSeries),
            ("if abridged", Cond::IfAbridged),
        ];
        list.into_iter()
            .map(|(name, cond)| {
                let re = Regex::new(&format!(r"^<(?P<not>!)?{}->", tag_name_regex(name))).unwrap();
                (re, cond)
            })
            .collect()
    })
}

/// Cached prefix regex for `<has`, `<is`, or `<cmp`, including an optional `!`.
fn checked_name_matcher(name: &str) -> &'static Regex {
    // Cheap per-name cache via boxed leak is overkill; build once for the three.
    match name {
        "has" => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new(r"^<(?P<not>!)?has").unwrap())
        }
        "is" => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new(r"^<(?P<not>!)?is").unwrap())
        }
        _ => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new(r"^<(?P<not>!)?cmp").unwrap())
        }
    }
}

/// Consumes a leading open conditional (`<if series->`, `<has …->`, …) or returns `None`.
fn match_open_conditional(rest: &str) -> Option<(usize, bool, Cond)> {
    if !rest.starts_with('<') {
        return None;
    }
    // Boolean conditionals (require immediate `->`).
    for (re, cond) in bool_conditionals() {
        if let Some(caps) = re.captures(rest) {
            let not = caps.name("not").is_some();
            return Some((caps.get(0).unwrap().end(), not, cond.clone()));
        }
    }
    // has / is / cmp.
    for name in ["has", "is", "cmp"] {
        let re = checked_name_matcher(name);
        let Some(caps) = re.captures(rest) else {
            continue;
        };
        let not = caps.name("not").is_some();
        let after = caps.get(0).unwrap().end();
        let Some(arrow) = find_arrow(rest, after) else {
            continue;
        };
        let rest_str = &rest[after..arrow];
        let consumed = arrow + 2;
        match name {
            "has" => {
                let property = match parse_optional_property(rest_str) {
                    Ok(p) => p,
                    Err(()) => continue,
                };
                return Some((consumed, not, Cond::Has { property }));
            }
            "is" => {
                let (property, check) = match parse_is_body(rest_str) {
                    Ok(v) => v,
                    Err(()) => continue,
                };
                return Some((consumed, not, Cond::Is { property, check }));
            }
            _ => {
                let Some((p1, op, p2)) = parse_cmp_body(rest_str) else {
                    continue;
                };
                return Some((consumed, not, Cond::Cmp { p1, op, p2 }));
            }
        }
    }
    None
}

/// Consumes `<-name>` or legacy `<end if>`; the third value is `true` for the legacy form.
fn match_close(rest: &str) -> Option<(usize, String, bool)> {
    if !rest.starts_with("<-") && !rest.starts_with("<end") {
        return None;
    }
    // Legacy `<end if>`.
    static END_IF: OnceLock<Regex> = OnceLock::new();
    let end_if = END_IF.get_or_init(|| Regex::new(r"^<end\s*if>").unwrap());
    if let Some(m) = end_if.find(rest) {
        return Some((m.end(), String::new(), true));
    }
    // New `<-name>`.
    static CLOSE: OnceLock<Regex> = OnceLock::new();
    let close = CLOSE.get_or_init(|| Regex::new(r"^<-\s*([^>]+?)\s*>").unwrap());
    if let Some(caps) = close.captures(rest) {
        let name = caps.get(1).unwrap().as_str();
        let canon = canonical(name);
        return Some((caps.get(0).unwrap().end(), canon, false));
    }
    None
}

/// Consumes a legacy `<if series>` (no `->`) open tag, or `None` for unknown names.
fn match_legacy_open(rest: &str) -> Option<(usize, Cond)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^<if\s+([a-zA-Z]+)>").unwrap());
    let caps = re.captures(rest)?;
    let name = caps.get(1).unwrap().as_str().to_lowercase();
    let cond = match name.as_str() {
        "series" => Cond::IfSeries,
        "podcast" => Cond::IfPodcast,
        "podcastparent" => Cond::IfPodcastParent,
        "bookseries" => Cond::IfBookseries,
        "abridged" => Cond::IfAbridged,
        "subtitle" => Cond::Has {
            property: Some("audible subtitle".into()),
        },
        "narrator" | "narrators" => Cond::Has {
            property: Some("narrator".into()),
        },
        "categories" => Cond::Has {
            property: Some("categories".into()),
        },
        _ => return None,
    };
    Some((caps.get(0).unwrap().end(), cond))
}

/// Consumes a legacy `%alias%` property tag when the alias is a known property.
fn match_percent(rest: &str) -> Option<(usize, String, Option<String>)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^%([^%]+)%").unwrap());
    let caps = re.captures(rest)?;
    let inner = caps.get(1).unwrap().as_str();
    let canon = tags::alias(&canonical(inner)).to_string();
    if !tags::is_known(&canon) {
        return None;
    }
    Some((caps.get(0).unwrap().end(), canon, None))
}

// ---------------------------------------------------------------------------
// Conditional body parsing helpers
// ---------------------------------------------------------------------------

/// Find the byte index of the first top-level `->` at or after `start`.
fn find_arrow(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    while i < s.len() {
        let c = bytes[i];
        if c == b'\\' {
            i += 2;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'[' => depth += 1,
            b']' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b'-' if depth == 0 && i + 1 < s.len() && bytes[i + 1] == b'>' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// `has` / bare property: optional, preceded by whitespace.
fn parse_optional_property(rest_str: &str) -> Result<Option<String>, ()> {
    if rest_str.is_empty() {
        return Ok(None);
    }
    if !rest_str.starts_with(char::is_whitespace) {
        return Err(());
    }
    let t = rest_str.trim();
    Ok(if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    })
}

/// `is` body: `\s+ property (?:\[check\])?` where the *last* top-level bracket
/// group is the check and everything before it is the property.
fn parse_is_body(rest_str: &str) -> Result<(Option<String>, Option<String>), ()> {
    if rest_str.is_empty() {
        return Ok((None, None));
    }
    if !rest_str.starts_with(char::is_whitespace) {
        return Err(());
    }
    let t = rest_str.trim();
    if t.is_empty() {
        return Ok((None, None));
    }
    match last_bracket_group(t) {
        Some((start, content)) => {
            let property = t[..start].trim_end();
            if property.is_empty() {
                Ok((Some(t.to_string()), None))
            } else {
                // Mirror `\[\s*` : leading whitespace inside the bracket is dropped.
                let check = content.trim_start_matches(char::is_whitespace);
                Ok((Some(property.to_string()), Some(check.to_string())))
            }
        }
        None => Ok((Some(t.to_string()), None)),
    }
}

/// Return `(start_byte, content)` of the last top-level `[...]` group in `t`
/// if it ends exactly at `t.len()`.
fn last_bracket_group(t: &str) -> Option<(usize, String)> {
    let bytes = t.as_bytes();
    let mut i = 0;
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    let mut group_start: Option<usize> = None;
    let mut last: Option<(usize, usize)> = None; // (start, end_exclusive)
    while i < t.len() {
        let c = bytes[i];
        if c == b'\\' {
            i += 2;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'[' => {
                if depth == 0 {
                    group_start = Some(i);
                }
                depth += 1;
            }
            b']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = group_start.take() {
                        last = Some((s, i + 1));
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    let (start, end) = last?;
    if end == t.len() {
        // content between the brackets
        let content = t[start + 1..end - 1].to_string();
        Some((start, content))
    } else {
        None
    }
}

/// `cmp` body: `property OP second_property`. The operator is the first
/// whitespace-delimited token (after the first property) that is entirely
/// symbolic operator chars or a `:named:` operator.
fn parse_cmp_body(rest_str: &str) -> Option<(String, String, String)> {
    if !rest_str.starts_with(char::is_whitespace) {
        return None;
    }
    let t = rest_str.trim();
    let tokens = top_level_tokens(t);
    for (idx, (start, end)) in tokens.iter().enumerate() {
        if idx == 0 {
            continue;
        }
        let tok = &t[*start..*end];
        if crate::compare::is_operator(tok) {
            let p1 = t[..*start].trim();
            let p2 = t[*end..].trim();
            if p1.is_empty() || p2.is_empty() {
                return None;
            }
            return Some((p1.to_string(), tok.to_string(), p2.to_string()));
        }
    }
    None
}

/// Split `t` into whitespace-delimited token byte ranges, respecting quotes and
/// bracket depth. Operates on chars so multi-byte operator glyphs stay intact.
fn top_level_tokens(t: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    let mut tok_start: Option<usize> = None;
    let mut escape = false;
    for (i, c) in t.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        let is_ws = quote.is_none() && depth == 0 && c.is_whitespace();
        if is_ws {
            if let Some(s) = tok_start.take() {
                tokens.push((s, i));
            }
            continue;
        }
        if tok_start.is_none() {
            tok_start = Some(i);
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            _ => {}
        }
    }
    if let Some(s) = tok_start.take() {
        tokens.push((s, t.len()));
    }
    tokens
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Evaluates the AST against book/chapter context into literal and tag fragments.
pub(crate) fn evaluate_parts(
    template: &Template,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
) -> Vec<TemplatePart> {
    let mut out = Vec::new();
    eval_nodes(&template.nodes, book, chapter, &mut out);
    out
}

/// Walks AST nodes, appending rendered fragments; false conditionals emit nothing.
fn eval_nodes(
    nodes: &[Node],
    book: &BookContext,
    chapter: Option<&ChapterContext>,
    out: &mut Vec<TemplatePart>,
) {
    for node in nodes {
        match node {
            Node::Literal(s) => {
                if !s.is_empty() {
                    out.push(TemplatePart {
                        value: s.clone(),
                        is_literal: true,
                    });
                }
            }
            Node::Property { canon, format } => {
                let value =
                    eval_display(canon, format.as_deref(), book, chapter).unwrap_or_default();
                out.push(TemplatePart {
                    value,
                    is_literal: false,
                });
            }
            Node::Conditional {
                not,
                cond,
                children,
            } => {
                let mut result = eval_cond(cond, book, chapter);
                if *not {
                    result = !result;
                }
                if result {
                    eval_nodes(children, book, chapter, out);
                }
            }
        }
    }
}

/// Evaluates a condition against the current book and optional chapter context.
fn eval_cond(cond: &Cond, book: &BookContext, chapter: Option<&ChapterContext>) -> bool {
    match cond {
        Cond::IfSeries => book.is_series() || book.is_podcast_parent(),
        Cond::IfPodcast => book.is_podcast() || book.is_podcast_parent(),
        Cond::IfPodcastParent => book.is_podcast_parent(),
        Cond::IfBookseries => book.is_series() && !book.is_podcast() && !book.is_podcast_parent(),
        Cond::IfAbridged => book.is_abridged,
        Cond::Has { property } => {
            let prop = property.as_deref().unwrap_or("");
            let v = resolve_property(prop, book, chapter);
            has_value(&v)
        }
        Cond::Is { property, check } => {
            let prop = property.as_deref().unwrap_or("");
            let v = resolve_property(prop, book, chapter);
            eval_is(check.as_deref(), &v)
        }
        Cond::Cmp { p1, op, p2 } => {
            let v1 = resolve_property(p1, book, chapter);
            let v2 = resolve_property(p2, book, chapter);
            eval_op(op, &v1, &v2)
        }
    }
}
