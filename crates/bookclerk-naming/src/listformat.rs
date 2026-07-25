//! Port of `LibationFileManager.Templates.IListFormat` — the list pipeline that
//! applies `sort`, `filter`, `unique`, `slice`, `max`, `count`, `separator` and
//! per-item `format(...)` to author / narrator / series / tag lists.

use crate::compare::{eval_op, try_get_literal};
use crate::dotnet_format::float_formatter;
use crate::template_string::{collapse_spaces_and_trim, unescape};
use crate::value::Value;

pub(crate) trait ListItem {
    /// `ToString(format)` — format the item using a `{TAG}` template (or default).
    fn to_string_fmt(&self, format: Option<&str>) -> String;
    /// Sort selector for a single (upper-cased) token.
    fn sort_key(&self, token: &str) -> String;
}

/// Valid format tokens per list type (used to decide whether `format(...)` is real).
#[derive(Clone, Copy)]
pub(crate) enum ListKind {
    Name,
    Series,
    StringList,
}

impl ListKind {
    fn tokens(self) -> &'static [&'static str] {
        match self {
            ListKind::Name => &["T", "F", "M", "L", "S", "ID"],
            ListKind::Series => &["#", "N", "ID"],
            ListKind::StringList => &["S"],
        }
    }

    fn sort_tokens(self) -> &'static [&'static str] {
        match self {
            ListKind::Name => &["T", "F", "M", "L", "S", "ID"],
            ListKind::Series => &["#", "N", "ID"],
            ListKind::StringList => &["S"],
        }
    }
}

/// Extract the content of a `name(...)` command, respecting `\` escapes and
/// quotes, stopping at the first unescaped, unquoted `)`.
fn command_content(s: &str, name: &str) -> Option<String> {
    let lower = s.to_lowercase();
    let needle = format!("{}(", name.to_lowercase());
    let start = lower.find(&needle)? + needle.len();
    let chars: Vec<char> = s.chars().collect();
    // Convert byte offset to char offset.
    let start_char = s[..start].chars().count();
    let mut out = String::new();
    let mut i = start_char;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                out.push(chars[i]);
                out.push(chars[i + 1]);
                i += 2;
            }
            '\'' | '"' => {
                let quote = chars[i];
                out.push(quote);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            ')' => return Some(out),
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    None
}

fn extract_format(format_string: &str, kind: ListKind) -> Option<String> {
    let content = command_content(format_string, "format")?;
    // Must contain a valid uppercase token like {L} or {N} or {#}.
    if kind
        .tokens()
        .iter()
        .any(|t| content.contains(&format!("{{{t}}}")) || content.contains(&format!("{{{t}:")))
    {
        Some(content)
    } else {
        None
    }
}

fn extract_separator(format_string: &str) -> Option<String> {
    command_content(format_string, "separator").map(|c| unescape(&c))
}

fn extract_count(format_string: &str) -> Option<String> {
    command_content(format_string, "count")
}

fn extract_unique(format_string: &str) -> Option<String> {
    command_content(format_string, "unique")
}

fn extract_max(format_string: &str) -> Option<usize> {
    let content = command_content(format_string, "max")?;
    content.trim().parse::<usize>().ok()
}

struct Slice {
    first: i64,
    last: i64,
    has_op: bool,
}

fn extract_slice(format_string: &str) -> Option<Slice> {
    let content = command_content(format_string, "slice")?;
    let content = content.trim();
    // Parse: first? op? last?
    let (first_str, rest) = take_int(content);
    let rest = rest.trim_start();
    let (op, rest) = if rest.starts_with("..") {
        let op_len = rest.chars().take_while(|c| *c == '.').count();
        (true, &rest[op_len..])
    } else {
        (false, rest)
    };
    let (last_str, _) = take_int(rest.trim_start());
    let first = first_str.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let mut last = last_str.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    if !op {
        last = first;
    }
    Some(Slice {
        first,
        last,
        has_op: op,
    })
}

fn take_int(s: &str) -> (Option<String>, &str) {
    let mut chars = s.char_indices().peekable();
    let mut end = 0;
    let mut started = false;
    if let Some(&(_, c)) = chars.peek() {
        if c == '-' {
            chars.next();
            end = 1;
            started = true;
        }
    }
    let mut has_digit = false;
    for (idx, c) in chars {
        if c.is_ascii_digit() {
            has_digit = true;
            end = idx + c.len_utf8();
        } else {
            break;
        }
    }
    if !has_digit {
        return (None, s);
    }
    let _ = started;
    (Some(s[..end].to_string()), &s[end..])
}

struct SortToken {
    token: String,
    descending: bool,
}

fn extract_sort(format_string: &str, kind: ListKind) -> Vec<SortToken> {
    let Some(content) = command_content(format_string, "sort") else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // Try each valid token (longest first: ID before single chars).
        let mut matched = None;
        for tok in kind.sort_tokens() {
            let tlen = tok.chars().count();
            if i + tlen <= chars.len() {
                let candidate: String = chars[i..i + tlen].iter().collect();
                if candidate.eq_ignore_ascii_case(tok) {
                    matched = Some((tok.to_string(), candidate));
                    break;
                }
            }
        }
        match matched {
            Some((canonical, matched_text)) => {
                let descending = matched_text.chars().all(|c| !c.is_uppercase());
                tokens.push(SortToken {
                    token: canonical.to_uppercase(),
                    descending,
                });
                i += matched_text.chars().count();
            }
            None => i += 1,
        }
    }
    tokens
}

/// Order items (stable) per the sort tokens.
fn sort_items(items: &mut [usize], all: &[&dyn ListItem], tokens: &[SortToken]) {
    for tok in tokens.iter().rev() {
        // Stable sort by this key; iterate in reverse so the first token is primary.
        items.sort_by(|&a, &b| {
            let ka = all[a].sort_key(&tok.token);
            let kb = all[b].sort_key(&tok.token);
            let ord = ka.cmp(&kb);
            if tok.descending {
                ord.reverse()
            } else {
                ord
            }
        });
    }
}

/// `FormattedList` — produce the list of formatted strings (not yet joined).
pub(crate) fn formatted_list(
    items: &[&dyn ListItem],
    format_string: Option<&str>,
    kind: ListKind,
) -> Vec<String> {
    let Some(fmt) = format_string else {
        return items.iter().map(|it| it.to_string_fmt(None)).collect();
    };

    // sort
    let mut order: Vec<usize> = (0..items.len()).collect();
    let sort_tokens = extract_sort(fmt, kind);
    if !sort_tokens.is_empty() {
        sort_items(&mut order, items, &sort_tokens);
    }

    // filter -> unique -> slice -> max
    order = apply_filter(order, items, fmt);
    order = apply_unique(order, items, fmt);
    order = apply_slice(order, fmt);
    order = apply_max(order, fmt);

    // count() short-circuits.
    if let Some(count_fmt) = extract_count(fmt) {
        if order.is_empty() {
            return Vec::new();
        }
        let cf = if count_fmt.is_empty() {
            None
        } else {
            Some(unescape(&count_fmt))
        };
        return vec![float_formatter(order.len() as f64, cf.as_deref())];
    }

    let item_format = extract_format(fmt, kind);
    let formatted: Vec<String> = order
        .iter()
        .map(|&i| items[i].to_string_fmt(item_format.as_deref()))
        .collect();

    if let Some(sep) = extract_separator(fmt) {
        match join_collapse(&sep, &formatted) {
            Some(joined) => vec![joined],
            None => Vec::new(),
        }
    } else {
        formatted
    }
}

/// Finalize a formatted list into a display string (join with `", "`).
pub(crate) fn finalize(list: &[String]) -> String {
    join_collapse(", ", list).unwrap_or_default()
}

fn join_collapse(sep: &str, items: &[String]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    Some(collapse_spaces_and_trim(&items.join(sep)))
}

fn apply_filter(order: Vec<usize>, items: &[&dyn ListItem], fmt: &str) -> Vec<usize> {
    let Some(content) = command_content(fmt, "filter") else {
        return order;
    };
    let Some((format, op, raw_value)) = parse_filter(&content) else {
        return order;
    };
    // Mirror `CommonFormatters.TryGetLiteral`: quoted -> string, digits -> int.
    let value_literal =
        try_get_literal(&raw_value).unwrap_or_else(|| Value::Str(unescape(&raw_value)));
    order
        .into_iter()
        .filter(|&i| {
            let formatted = items[i].to_string_fmt(format.as_deref());
            eval_op(&op, &Value::Str(formatted), &value_literal)
        })
        .collect()
}

/// Parse filter content into `(format, op, raw_value)` where `raw_value` still
/// carries its quotes so [`try_get_literal`] can classify it.
fn parse_filter(content: &str) -> Option<(Option<String>, String, String)> {
    let chars: Vec<char> = content.chars().collect();
    // Trailing whitespace is not part of the value.
    let mut end = chars.len();
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }

    let (value_start, raw_value): (usize, String) =
        if chars[end - 1] == '\'' || chars[end - 1] == '"' {
            // Value is a quoted string; find its opening quote by forward-scanning
            // the whole content and taking the quoted region that ends at `end`.
            let start = quoted_region_start(&chars, end)?;
            let raw: String = chars[start..end].iter().collect();
            (start, raw)
        } else if chars[end - 1].is_ascii_digit() {
            let mut s = end;
            while s > 0 && chars[s - 1].is_ascii_digit() {
                s -= 1;
            }
            let raw: String = chars[s..end].iter().collect();
            (s, raw)
        } else {
            return None;
        };

    // Op = trailing operator run before the value.
    let mut op_end = value_start;
    while op_end > 0 && chars[op_end - 1].is_whitespace() {
        op_end -= 1;
    }
    let mut op_start = op_end;
    while op_start > 0 && is_op_char(chars[op_start - 1]) {
        op_start -= 1;
    }
    let op: String = chars[op_start..op_end].iter().collect();
    let format_raw: String = chars[..op_start].iter().collect();
    let format = format_raw.trim();
    let format = if format.is_empty() {
        None
    } else {
        Some(format.to_string())
    };
    Some((format, op, raw_value))
}

/// Forward-scan `chars` (respecting `\` escapes and doubled quotes) and return
/// the start index of the quoted region whose closing quote sits at `end - 1`.
fn quoted_region_start(chars: &[char], end: usize) -> Option<usize> {
    let mut i = 0;
    while i < end {
        let c = chars[i];
        if c == '\\' {
            i += 2;
            continue;
        }
        if c == '\'' || c == '"' {
            let quote = c;
            let start = i;
            i += 1;
            while i < end {
                if chars[i] == quote {
                    // Doubled quote inside the string -> literal quote.
                    if i + 1 < end && chars[i + 1] == quote {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            if i == end {
                return Some(start);
            }
            continue;
        }
        i += 1;
    }
    None
}

fn is_op_char(c: char) -> bool {
    matches!(
        c,
        '#' | '!'
            | '≡'
            | '='
            | '≠'
            | '~'
            | '<'
            | '>'
            | '≤'
            | '≥'
            | '&'
            | '∉'
            | '∌'
            | '∈'
            | '⋂'
            | '⊆'
            | '⊇'
            | '⊂'
            | '⊃'
            | '-'
            | ':'
            | '\u{338}'
    ) || c.is_ascii_lowercase()
}

fn apply_unique(order: Vec<usize>, items: &[&dyn ListItem], fmt: &str) -> Vec<usize> {
    let Some(unique_fmt) = extract_unique(fmt) else {
        return order;
    };
    let fmt_opt = if unique_fmt.is_empty() {
        None
    } else {
        Some(unique_fmt.as_str())
    };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for i in order {
        let key = items[i].to_string_fmt(fmt_opt).to_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            out.push(i);
        }
    }
    out
}

fn apply_slice(order: Vec<usize>, fmt: &str) -> Vec<usize> {
    let Some(slice) = extract_slice(fmt) else {
        return order;
    };
    let count = order.len() as i64;
    let mut first = slice.first;
    let last = slice.last;
    let mut items = order;

    if last > 0 {
        if first < 0 {
            first += count + 1;
        }
        items = take(items, last as usize);
    }
    if first > 1 {
        items = skip(items, (first - 1) as usize);
    } else if first < 0 {
        items = take_last(items, (-first) as usize);
    }
    if last < -1 {
        items = skip_last(items, (-last - 1) as usize);
    }
    let _ = slice.has_op;
    items
}

fn take(mut v: Vec<usize>, n: usize) -> Vec<usize> {
    v.truncate(n);
    v
}
fn skip(v: Vec<usize>, n: usize) -> Vec<usize> {
    v.into_iter().skip(n).collect()
}
fn take_last(v: Vec<usize>, n: usize) -> Vec<usize> {
    let len = v.len();
    let start = len.saturating_sub(n);
    v.into_iter().skip(start).collect()
}
fn skip_last(v: Vec<usize>, n: usize) -> Vec<usize> {
    let len = v.len();
    let keep = len.saturating_sub(n);
    v.into_iter().take(keep).collect()
}

fn apply_max(order: Vec<usize>, fmt: &str) -> Vec<usize> {
    match extract_max(fmt) {
        Some(max) => take(order, max),
        None => order,
    }
}
