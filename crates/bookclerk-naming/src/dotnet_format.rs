//! Ports of the numeric / string / date formatting helpers from
//! `FileManager.NamingTemplate.CommonFormatters`.

use chrono::{Datelike, NaiveDateTime, Timelike};
use regex::Regex;
use std::sync::OnceLock;

/// Fallback .NET date pattern (`yyyy-MM-dd`) when the caller omits a format.
pub(crate) const DEFAULT_DATE_FORMAT: &str = "yyyy-MM-dd";

/// Cached regex for optional left-truncate length plus `u`/`l`/`t`/`T` case flags.
fn string_format_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?P<left>\d+)?\s*(?P<case>[uUlLtT])?\s*$").unwrap())
}

/// Port of `CommonFormatters._StringFormatter`.
pub(crate) fn string_formatter(value: &str, format: Option<&str>) -> String {
    if value.is_empty() {
        return String::new();
    }
    let Some(fmt) = format else {
        return value.to_string();
    };
    if fmt.trim().is_empty() {
        return value.to_string();
    }
    let Some(caps) = string_format_re().captures(fmt) else {
        return value.to_string();
    };

    let mut value = value.to_string();
    if let Some(left) = caps.name("left") {
        if let Ok(len) = left.as_str().parse::<usize>() {
            if len < value.chars().count() {
                value = value.chars().take(len).collect();
            }
        }
    }

    match caps.name("case").map(|m| m.as_str()) {
        Some("u") | Some("U") => value.to_uppercase(),
        Some("l") | Some("L") => value.to_lowercase(),
        Some("T") => to_title_case(&value),
        Some("t") => to_title_case(&value.to_lowercase()),
        _ => value,
    }
}

/// Approximate `TextInfo.ToTitleCase`: capitalize the first letter of each word,
/// lowercase the remainder, but leave all-uppercase words (acronyms) untouched.
fn to_title_case(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut word = String::new();

    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let is_acronym = word.chars().all(|c| !c.is_lowercase());
        if is_acronym {
            out.push_str(word);
        } else {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                for c in chars {
                    out.extend(c.to_lowercase());
                }
            }
        }
        word.clear();
    };

    for ch in value.chars() {
        if ch.is_alphabetic() {
            word.push(ch);
        } else {
            flush(&mut word, &mut out);
            out.push(ch);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Port of `CommonFormatters._FloatFormatter`.
pub(crate) fn float_formatter(value: f64, format: Option<&str>) -> String {
    match format {
        None => default_number(value),
        Some(fmt) => {
            // .NET `int.TryParse` accepts surrounding whitespace, so `[ 4 ]`
            // behaves like `[4]` (zero-pad to 4 digits).
            if let Ok(digits) = fmt.trim().parse::<i64>() {
                if digits > 0 {
                    // Zero-pad the integer part (with an optional fraction).
                    let pattern = format!("{}.################", "0".repeat(digits as usize));
                    return format_number(value, &pattern);
                }
            }
            if fmt.is_empty() {
                default_number(value)
            } else {
                format_number(value, fmt)
            }
        }
    }
}

/// Formats a float without a fraction when it is an integer whose magnitude is below `1e15`.
fn default_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Port of `CommonFormatters.DateTimeFormatter`.
pub(crate) fn datetime_formatter(value: NaiveDateTime, format: Option<&str>) -> String {
    let fmt = match format {
        Some(f) if !f.trim().is_empty() => f,
        _ => DEFAULT_DATE_FORMAT,
    };
    format_datetime(value, fmt)
}

// --- .NET-style numeric formatting ------------------------------------------

/// Format a number using a subset of .NET standard / custom numeric format
/// strings. Supports standard `F`/`N`/`D`, and custom patterns with `0`, `#`,
/// `.`, `,`, quoted literals and `\` escapes.
pub(crate) fn format_number(value: f64, format: &str) -> String {
    if let Some(std) = try_standard(value, format) {
        return std;
    }
    format_custom(value, format)
}

/// Applies .NET `F`/`N`/`D` standard numeric formats; `None` when the pattern is custom.
fn try_standard(value: f64, format: &str) -> Option<String> {
    let mut chars = format.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let rest: String = chars.collect();
    if !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let precision: Option<usize> = if rest.is_empty() {
        None
    } else {
        rest.parse().ok()
    };
    match letter {
        'F' | 'f' => {
            let p = precision.unwrap_or(2);
            Some(format!("{value:.p$}"))
        }
        'N' | 'n' => {
            let p = precision.unwrap_or(2);
            Some(group_thousands(&format!("{value:.p$}")))
        }
        'D' | 'd' => {
            let width = precision.unwrap_or(0);
            let neg = value < 0.0;
            let digits = format!("{:0>width$}", (value.abs().round() as i64), width = width);
            Some(if neg { format!("-{digits}") } else { digits })
        }
        _ => None,
    }
}

/// Inserts commas into the integer part of a decimal string (preserves a leading minus).
fn group_thousands(num: &str) -> String {
    let (int_part, frac_part) = match num.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (num, None),
    };
    let neg = int_part.starts_with('-');
    let digits = int_part.trim_start_matches('-');
    let mut grouped = String::new();
    let count = digits.len();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (count - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&grouped);
    if let Some(f) = frac_part {
        out.push('.');
        out.push_str(f);
    }
    out
}

#[derive(Debug)]
/// One token of a custom numeric pattern (`0`, `#`, `,`, or a quoted/escaped literal).
enum NumToken {
    /// Required digit placeholder (`0`); pads with zero when no digit remains.
    Zero,
    /// Optional digit placeholder (`#`); omitted when no digit remains.
    Hash,
    /// Quoted, escaped, or other literal text copied into the output.
    Literal(String),
    /// Thousands-separator placeholder (`,`) in the integer pattern.
    Group,
}

/// Splits a custom numeric pattern into integer and fractional token lists at `.`.
fn tokenize_number_format(format: &str) -> (Vec<NumToken>, Vec<NumToken>) {
    let mut int_tokens = Vec::new();
    let mut frac_tokens = Vec::new();
    let mut in_frac = false;
    let mut chars = format.chars().peekable();

    while let Some(ch) = chars.next() {
        let target = if in_frac {
            &mut frac_tokens
        } else {
            &mut int_tokens
        };
        match ch {
            '0' => target.push(NumToken::Zero),
            '#' => target.push(NumToken::Hash),
            ',' => {
                if !in_frac {
                    int_tokens.push(NumToken::Group);
                }
            }
            '.' if !in_frac => {
                in_frac = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    target.push(NumToken::Literal(next.to_string()));
                }
            }
            '\'' | '"' => {
                let quote = ch;
                let mut lit = String::new();
                for c in chars.by_ref() {
                    if c == quote {
                        break;
                    }
                    lit.push(c);
                }
                target.push(NumToken::Literal(lit));
            }
            other => target.push(NumToken::Literal(other.to_string())),
        }
    }
    (int_tokens, frac_tokens)
}

/// Renders `value` with custom `0`/`#`/`,` placeholders and optional grouping.
fn format_custom(value: f64, format: &str) -> String {
    let (int_tokens, frac_tokens) = tokenize_number_format(format);

    let int_zeros = int_tokens
        .iter()
        .filter(|t| matches!(t, NumToken::Zero))
        .count();
    let frac_slots = frac_tokens
        .iter()
        .filter(|t| matches!(t, NumToken::Zero | NumToken::Hash))
        .count();
    let has_grouping = int_tokens.iter().any(|t| matches!(t, NumToken::Group));

    let neg = value < 0.0;
    let rounded =
        (value.abs() * 10f64.powi(frac_slots as i32)).round() / 10f64.powi(frac_slots as i32);
    let s = format!("{:.*}", frac_slots, rounded);
    let (int_digits, frac_digits) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (s, String::new()),
    };

    // Integer part: pad to min zeros.
    let mut int_digits = int_digits;
    while int_digits.len() < int_zeros {
        int_digits.insert(0, '0');
    }

    let int_out = render_integer(&int_tokens, &int_digits, has_grouping);
    let frac_out = render_fraction(&frac_tokens, &frac_digits);

    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&int_out);
    if !frac_out.is_empty() {
        out.push('.');
        out.push_str(&frac_out);
    }
    out
}

/// Walks integer tokens right-to-left, flushing leftover digits on the leftmost placeholder.
fn render_integer(tokens: &[NumToken], digits: &str, has_grouping: bool) -> String {
    let digit_chars: Vec<char> = digits.chars().collect();
    let placeholder_count = tokens
        .iter()
        .filter(|t| matches!(t, NumToken::Zero | NumToken::Hash))
        .count();

    // Identify the index (in tokens) of the leftmost placeholder.
    let leftmost = tokens
        .iter()
        .position(|t| matches!(t, NumToken::Zero | NumToken::Hash));

    let mut out_rev: Vec<char> = Vec::new();
    let mut di = digit_chars.len();
    let mut seen_placeholder = 0usize;

    for (idx, token) in tokens.iter().enumerate().rev() {
        match token {
            NumToken::Literal(s) => {
                for c in s.chars().rev() {
                    out_rev.push(c);
                }
            }
            NumToken::Group => { /* handled via has_grouping fallback below */ }
            NumToken::Zero | NumToken::Hash => {
                seen_placeholder += 1;
                let is_leftmost = Some(idx) == leftmost;
                if is_leftmost {
                    // Flush all remaining digits here.
                    if di > 0 {
                        for c in digit_chars[..di].iter().rev() {
                            out_rev.push(*c);
                        }
                        di = 0;
                    } else if matches!(token, NumToken::Zero) {
                        out_rev.push('0');
                    }
                } else if di > 0 {
                    di -= 1;
                    out_rev.push(digit_chars[di]);
                } else if matches!(token, NumToken::Zero) {
                    out_rev.push('0');
                }
            }
        }
    }
    let _ = (placeholder_count, seen_placeholder);

    let out: String = out_rev.into_iter().rev().collect();

    if has_grouping {
        // Simplified grouping: group the raw integer digits and keep leading /
        // trailing literals. Adequate for `#,##0`-style patterns.
        let mut leading = String::new();
        let mut trailing = String::new();
        for token in tokens {
            if let NumToken::Literal(s) = token {
                if out.starts_with(s.as_str()) || leading.is_empty() {
                    // best-effort; grouping paths are only used by culture tests
                }
            }
            let _ = &mut leading;
            let _ = &mut trailing;
        }
        return group_thousands(&out);
    }

    out
}

/// Walks fractional tokens left-to-right and strips trailing `#` zeros.
fn render_fraction(tokens: &[NumToken], digits: &str) -> String {
    let digit_chars: Vec<char> = digits.chars().collect();
    let mut out = String::new();
    let mut di = 0usize;
    // Track trailing removable characters (from '#' placeholders that got 0).
    let mut removable_from: Option<usize> = None;

    for token in tokens {
        match token {
            NumToken::Literal(s) => out.push_str(s),
            NumToken::Group => {}
            NumToken::Zero => {
                let c = digit_chars.get(di).copied().unwrap_or('0');
                di += 1;
                out.push(c);
                removable_from = None;
            }
            NumToken::Hash => {
                let c = digit_chars.get(di).copied().unwrap_or('0');
                di += 1;
                if c == '0' {
                    if removable_from.is_none() {
                        removable_from = Some(out.len());
                    }
                    out.push(c);
                } else {
                    out.push(c);
                    removable_from = None;
                }
            }
        }
    }

    if let Some(idx) = removable_from {
        out.truncate(idx);
    }
    out
}

// --- .NET-style date formatting ---------------------------------------------

/// Expands .NET date tokens (`y`/`M`/`d`/`H`/`h`/`m`/`s`/`t`/`f`) and quoted literals.
fn format_datetime(value: NaiveDateTime, format: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        match ch {
            'y' | 'M' | 'd' | 'H' | 'h' | 'm' | 's' | 't' | 'f' => {
                let mut count = 1;
                while i + count < chars.len() && chars[i + count] == ch {
                    count += 1;
                }
                out.push_str(&format_date_token(value, ch, count));
                i += count;
            }
            '\\' => {
                if i + 1 < chars.len() {
                    out.push(chars[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '\'' | '"' => {
                let quote = ch;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    out.push(chars[i]);
                    i += 1;
                }
                i += 1; // skip closing quote
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

/// Formats one repeated date specifier; width follows the token's character count.
fn format_date_token(value: NaiveDateTime, ch: char, count: usize) -> String {
    match ch {
        'y' => match count {
            1 => (value.year() % 100).to_string(),
            2 => format!("{:02}", value.year().rem_euclid(100)),
            3 => format!("{:03}", value.year()),
            _ => format!("{:04}", value.year()),
        },
        'M' => {
            if count >= 2 {
                format!("{:02}", value.month())
            } else {
                value.month().to_string()
            }
        }
        'd' => {
            if count >= 2 {
                format!("{:02}", value.day())
            } else {
                value.day().to_string()
            }
        }
        'H' => {
            if count >= 2 {
                format!("{:02}", value.hour())
            } else {
                value.hour().to_string()
            }
        }
        'h' => {
            let h12 = {
                let h = value.hour() % 12;
                if h == 0 {
                    12
                } else {
                    h
                }
            };
            if count >= 2 {
                format!("{h12:02}")
            } else {
                h12.to_string()
            }
        }
        'm' => {
            if count >= 2 {
                format!("{:02}", value.minute())
            } else {
                value.minute().to_string()
            }
        }
        's' => {
            if count >= 2 {
                format!("{:02}", value.second())
            } else {
                value.second().to_string()
            }
        }
        't' => {
            let am = value.hour() < 12;
            if count >= 2 {
                if am {
                    "AM".to_string()
                } else {
                    "PM".to_string()
                }
            } else if am {
                "A".to_string()
            } else {
                "P".to_string()
            }
        }
        'f' => {
            let nanos = value.and_utc().timestamp_subsec_nanos();
            let frac = format!("{nanos:09}");
            frac.chars().take(count).collect()
        }
        _ => String::new(),
    }
}
