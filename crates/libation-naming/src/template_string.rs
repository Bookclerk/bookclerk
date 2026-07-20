//! Port of `CommonFormatters.TemplateStringFormatter` — the `{TAG}` / `{pre{TAG}post}`
//! mini-template used to format individual list members (names, series, tags).

use crate::dotnet_format::string_formatter;
use crate::series_order::SeriesOrder;
use regex::Regex;
use std::sync::OnceLock;

pub(crate) enum ItemValue {
    Str(String),
    Series(SeriesOrder),
}

pub(crate) trait FormatItem {
    /// Look up a token (already upper-cased) and return its value.
    fn lookup(&self, token: &str) -> Option<ItemValue>;
}

fn collapse_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^ +| +(?:$| )").unwrap())
}

/// Collapse runs of spaces to a single space and trim leading/trailing spaces
/// (mirrors `CollapseSpacesAndTrimRegex`).
pub(crate) fn collapse_spaces_and_trim(input: &str) -> String {
    // The C# regex ` +(?=$| )` uses lookahead; emulate by iterating.
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    // Leading spaces.
    while i < chars.len() && chars[i] == ' ' {
        i += 1;
    }
    while i < chars.len() {
        if chars[i] == ' ' {
            // Count the run.
            let mut j = i;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            if j >= chars.len() {
                // trailing spaces -> drop
            } else {
                out.push(' ');
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    let _ = collapse_re();
    out
}

/// Port of `CommonFormatters.Unescape` with quote chars `'` and `"`.
pub(crate) fn unescape(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            while i < chars.len() {
                let inner = chars[i];
                if inner == quote {
                    i += 1;
                    // Doubled quote -> literal quote.
                    if i < chars.len() && chars[i] == quote {
                        out.push(quote);
                        i += 1;
                        continue;
                    }
                    break;
                }
                out.push(inner);
                i += 1;
            }
            continue;
        }
        if c == '\\' && i + 1 < chars.len() {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn format_value(value: ItemValue, format: Option<&str>) -> String {
    match value {
        ItemValue::Str(s) => string_formatter(&s, format),
        ItemValue::Series(order) => order.to_display(format),
    }
}

/// Format `item` using `template`, resolving `{TAG}` tokens.
pub(crate) fn format(item: &dyn FormatItem, template: &str) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut gap = String::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '{' {
            if let Some((consumed, rendered)) = try_parse_tag(&chars, i, item) {
                out.push_str(&unescape(&gap));
                gap.clear();
                out.push_str(&rendered);
                i += consumed;
                continue;
            }
        }
        gap.push(chars[i]);
        i += 1;
    }
    out.push_str(&unescape(&gap));

    collapse_spaces_and_trim(&out)
}

/// Attempt to parse a `{TAG}` (simple) or `{pre{TAG}post}` (wrapped) construct
/// starting at `start` (which must be `{`). Returns (chars_consumed, rendered).
fn try_parse_tag(chars: &[char], start: usize, item: &dyn FormatItem) -> Option<(usize, String)> {
    // Try simple: {TAG(@lang)?(:format)?}
    if let Some((end, tag, format)) = parse_inner(chars, start) {
        let rendered = render(item, &tag, format.as_deref(), "", "");
        return Some((end - start, rendered));
    }

    // Try wrapped: { PRE { TAG(:format)? } POST }
    let mut i = start + 1;
    let mut pre = String::new();
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                pre.push(chars[i]);
                pre.push(chars[i + 1]);
                i += 2;
            }
            '\'' | '"' => {
                let quote = chars[i];
                pre.push(quote);
                i += 1;
                while i < chars.len() {
                    pre.push(chars[i]);
                    if chars[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '{' | '}' => break,
            other => {
                pre.push(other);
                i += 1;
            }
        }
    }
    if i >= chars.len() || chars[i] != '{' {
        return None;
    }
    let (inner_end, tag, format) = parse_inner(chars, i)?;
    // POST until unescaped '}'.
    let mut j = inner_end;
    let mut post = String::new();
    while j < chars.len() {
        match chars[j] {
            '\\' if j + 1 < chars.len() => {
                post.push(chars[j]);
                post.push(chars[j + 1]);
                j += 2;
            }
            '\'' | '"' => {
                let quote = chars[j];
                post.push(quote);
                j += 1;
                while j < chars.len() {
                    post.push(chars[j]);
                    if chars[j] == quote {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
            }
            '{' | '}' => break,
            other => {
                post.push(other);
                j += 1;
            }
        }
    }
    if j >= chars.len() || chars[j] != '}' {
        return None;
    }
    j += 1; // consume closing '}'
    let rendered = render(item, &tag, format.as_deref(), &pre, &post);
    Some((j - start, rendered))
}

/// Parse `{TAG(@lang)?(:format)?}` starting at `pos` (a `{`).
/// Returns (index-after-closing-brace, tag, format).
fn parse_inner(chars: &[char], pos: usize) -> Option<(usize, String, Option<String>)> {
    let mut i = pos + 1;
    // TAG: [A-Za-z0-9]+ or '#'
    let mut tag = String::new();
    if i < chars.len() && chars[i] == '#' {
        tag.push('#');
        i += 1;
    } else {
        while i < chars.len() && chars[i].is_ascii_alphanumeric() {
            tag.push(chars[i]);
            i += 1;
        }
    }
    if tag.is_empty() {
        return None;
    }
    // Optional @lang
    if i < chars.len() && chars[i] == '@' {
        i += 1;
        while i < chars.len() && (chars[i].is_ascii_alphabetic() || chars[i] == '-') {
            i += 1;
        }
    }
    // Optional :format
    let mut format: Option<String> = None;
    if i < chars.len() && chars[i] == ':' {
        i += 1;
        let mut fmt = String::new();
        while i < chars.len() {
            match chars[i] {
                '\\' if i + 1 < chars.len() => {
                    fmt.push(chars[i]);
                    fmt.push(chars[i + 1]);
                    i += 2;
                }
                '\'' | '"' => {
                    let quote = chars[i];
                    fmt.push(quote);
                    i += 1;
                    while i < chars.len() {
                        fmt.push(chars[i]);
                        if chars[i] == quote {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
                '}' => break,
                other => {
                    fmt.push(other);
                    i += 1;
                }
            }
        }
        format = Some(fmt);
    }
    if i >= chars.len() || chars[i] != '}' {
        return None;
    }
    i += 1; // consume '}'
    Some((i, tag, format))
}

fn render(item: &dyn FormatItem, tag: &str, format: Option<&str>, pre: &str, post: &str) -> String {
    let token = tag.to_uppercase();
    let Some(value) = item.lookup(&token) else {
        // Unknown tag: leave the original braces untouched.
        let mut s = String::from("{");
        s.push_str(tag);
        if let Some(f) = format {
            s.push(':');
            s.push_str(f);
        }
        s.push('}');
        return s;
    };
    let formatted = format_value(value, format);
    if formatted.is_empty() {
        String::new()
    } else {
        format!("{}{}{}", unescape(pre), formatted, unescape(post))
    }
}
