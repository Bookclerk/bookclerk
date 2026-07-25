//! Port of `LibationFileManager.Templates.SeriesOrder`.

use crate::dotnet_format::format_number;

#[derive(Debug, Clone)]
enum OrderPart {
    Text(String),
    Num(f64),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SeriesOrder {
    parts: Vec<OrderPart>,
}

impl SeriesOrder {
    pub fn parse(order: Option<&str>) -> Self {
        let mut parts = Vec::new();
        let mut remaining = order.map(str::to_string);

        while let Some(cur) = remaining.take() {
            if let Some((value, start, end)) = try_parse_number(&cur) {
                let prefix = &cur[..start];
                if !prefix.is_empty() {
                    parts.push(OrderPart::Text(prefix.to_string()));
                }
                parts.push(OrderPart::Num(value));
                let rest = cur[end..].to_string();
                if !rest.is_empty() {
                    remaining = Some(rest);
                }
            } else {
                if !cur.is_empty() {
                    parts.push(OrderPart::Text(cur));
                }
                break;
            }
        }

        Self { parts }
    }

    pub fn to_display(&self, format: Option<&str>) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                OrderPart::Text(s) => out.push_str(s),
                OrderPart::Num(f) => match format {
                    Some(fmt) if !fmt.trim().is_empty() => out.push_str(&format_number(*f, fmt)),
                    _ => out.push_str(&default_num(*f)),
                },
            }
        }
        out.trim().to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

fn default_num(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// Port of `SeriesOrder.TryParseNumber`: greedy positive number search that
/// returns the value plus its byte range within `s`.
fn try_parse_number(s: &str) -> Option<(f64, usize, usize)> {
    if s.trim().is_empty() {
        return None;
    }
    let bytes: Vec<(usize, char)> = s.char_indices().collect();
    let len = s.len();

    for si in 0..bytes.len() {
        let (start_byte, ch) = bytes[si];
        if !ch.is_ascii_digit() {
            continue;
        }
        // end iterates from string end down to just after start.
        let mut end_char = bytes.len();
        while end_char > si {
            // byte index of the end (exclusive)
            let end_byte = if end_char == bytes.len() {
                len
            } else {
                bytes[end_char].0
            };
            // Skip trailing whitespace (preserved in output text).
            let last_char = bytes[end_char - 1].1;
            if last_char.is_whitespace() {
                end_char -= 1;
                continue;
            }
            let substring = &s[start_byte..end_byte];
            if let Ok(v) = substring.parse::<f64>() {
                if v.is_finite() {
                    return Some((v, start_byte, end_byte));
                }
            }
            end_char -= 1;
        }
    }
    None
}
