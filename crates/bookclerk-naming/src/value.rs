//! Runtime value model for template tags and conditionals.

use chrono::{Datelike, NaiveDate, NaiveDateTime};

/// A resolved tag value, mirroring the `object?` values used by Bookclerk's
/// conditional evaluators.
#[derive(Debug, Clone)]
pub(crate) enum Value {
    Null,
    Str(String),
    Int(i64),
    /// A `TimeSpan` expressed as total minutes.
    Minutes(f64),
    Date(NaiveDateTime),
    /// A list of already-stringified members (names, series, tags, ...).
    List(Vec<String>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Port of `CompareCondition.ToIntObject`.
    pub fn to_int(&self) -> Option<i64> {
        match self {
            Value::Null => None,
            Value::List(v) => Some(v.len() as i64),
            Value::Minutes(m) => Some(*m as i64),
            Value::Date(d) => Some(oadate_days(*d)),
            Value::Str(s) => Some(s.chars().count() as i64),
            Value::Int(i) => Some(*i),
        }
    }

    /// Port of `CompareCondition.ValueToString`.
    pub fn to_compare_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Str(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::Minutes(m) => format!("{}", m.round() as i64),
            Value::Date(d) => d.format("%Y-%m-%d %H:%M:%S").to_string(),
            // Lists never string-equal a scalar; use a sentinel.
            Value::List(_) => "\u{0}__LIST__\u{0}".to_string(),
        }
    }

    /// Members of the value when treated as an enumerable (`ToEnumerable`).
    pub fn to_enumerable(&self) -> Vec<String> {
        match self {
            Value::List(v) => v.clone(),
            Value::Null => Vec::new(),
            other => vec![other.to_compare_string()],
        }
    }
}

/// Integer part of the OLE Automation date (days since 1899-12-30).
fn oadate_days(dt: NaiveDateTime) -> i64 {
    let base = NaiveDate::from_ymd_opt(1899, 12, 30).unwrap();
    (dt.date().num_days_from_ce() - base.num_days_from_ce()) as i64
}
