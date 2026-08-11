//! Port of `FileManager.NamingTemplate.CompareCondition`.

use crate::template_string::unescape;
use crate::value::Value;
use regex::RegexBuilder;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Num,
    List,
    Str,
}

/// Operators recognised by the engine, longest-first so prefix matching is
/// unambiguous. Each entry is `(token, kind, canonical)`.
fn operators() -> &'static [(&'static str, OpKind, &'static str)] {
    &[
        // List operators (named)
        (":not_contains:", OpKind::List, "not_contains"),
        (":proper_superset:", OpKind::List, "proper_superset"),
        (":proper_subset:", OpKind::List, "proper_subset"),
        (":superset:", OpKind::List, "superset"),
        (":contains:", OpKind::List, "contains"),
        (":disjoint:", OpKind::List, "disjoint"),
        (":overlaps:", OpKind::List, "overlaps"),
        (":subset:", OpKind::List, "subset"),
        (":equals:", OpKind::List, "equals"),
        (":not_in:", OpKind::List, "not_in"),
        (":in:", OpKind::List, "in"),
        // List operators (symbolic, multi-char)
        ("&&!", OpKind::List, "disjoint"),
        ("⋂!", OpKind::List, "disjoint"),
        ("⋂\u{338}", OpKind::List, "disjoint"),
        (">=>", OpKind::List, "superset"),
        (">->", OpKind::List, "proper_superset"),
        ("<=<", OpKind::List, "subset"),
        ("<-<", OpKind::List, "proper_subset"),
        ("!>>", OpKind::List, "not_contains"),
        ("!<<", OpKind::List, "not_in"),
        (">>", OpKind::List, "contains"),
        ("<<", OpKind::List, "in"),
        ("&&", OpKind::List, "overlaps"),
        ("∋", OpKind::List, "contains"),
        ("∌", OpKind::List, "not_contains"),
        ("∈", OpKind::List, "in"),
        ("∉", OpKind::List, "not_in"),
        ("⊆", OpKind::List, "subset"),
        ("⊇", OpKind::List, "superset"),
        ("⊂", OpKind::List, "proper_subset"),
        ("⊃", OpKind::List, "proper_superset"),
        ("⋂", OpKind::List, "overlaps"),
        ("≡", OpKind::List, "equals"),
        ("==", OpKind::List, "equals"),
        // Numeric operators
        ("#!=", OpKind::Num, "ne"),
        ("#>=", OpKind::Num, "ge"),
        ("#<=", OpKind::Num, "le"),
        ("#=", OpKind::Num, "eq"),
        ("#>", OpKind::Num, "gt"),
        ("#<", OpKind::Num, "lt"),
        ("≠", OpKind::Num, "ne"),
        ("≥", OpKind::Num, "ge"),
        ("≤", OpKind::Num, "le"),
        (">=", OpKind::Num, "ge"),
        ("<=", OpKind::Num, "le"),
        (">", OpKind::Num, "gt"),
        ("<", OpKind::Num, "lt"),
        // String operators
        ("=~", OpKind::Str, "regex"),
        ("!~", OpKind::Str, "not_regex"),
        ("~", OpKind::Str, "regex"),
        ("!=", OpKind::Str, "ne"),
        ("!", OpKind::Str, "ne"),
        ("=", OpKind::Str, "eq"),
    ]
}

/// True if `tok` is a recognised comparison operator token (used by the parser
/// to locate the operator inside a `cmp` body). The empty string is *not*
/// treated as an operator here.
pub(crate) fn is_operator(tok: &str) -> bool {
    !tok.is_empty() && classify(tok).is_some()
}

/// Classify a whole-token operator (used by `cmp` and `filter`).
fn classify(op: &str) -> Option<(OpKind, &'static str)> {
    for (tok, kind, canon) in operators() {
        if *tok == op {
            return Some((*kind, *canon));
        }
    }
    // Empty operator == string equality.
    if op.is_empty() {
        return Some((OpKind::Str, "eq"));
    }
    None
}

/// Match the leading operator of a check string, returning `(kind, canonical, value_rest)`.
fn match_leading(check: &str) -> (OpKind, &'static str, String) {
    for (tok, kind, canon) in operators() {
        if let Some(rest) = check.strip_prefix(tok) {
            return (*kind, *canon, rest.to_string());
        }
    }
    // No operator: empty string op, whole check is the value.
    (OpKind::Str, "eq", check.to_string())
}

/// Evaluate an `is` conditional: `check` is the bracket content (or `None`).
pub(crate) fn eval_is(check: Option<&str>, v1: &Value) -> bool {
    let Some(check) = check else {
        return existence(v1);
    };

    let (kind, canon, rest) = match_leading(check);
    match kind {
        OpKind::Num => {
            let v2 = Value::Int(rest.trim().parse::<i64>().unwrap_or(0));
            eval_num(canon, v1, &v2)
        }
        OpKind::List => {
            let v2 = Value::List(vec![unescape(&rest)]);
            eval_list(canon, v1, &v2)
        }
        OpKind::Str => {
            let v2 = Value::Str(unescape(&rest));
            eval_str(canon, v1, &v2)
        }
    }
}

/// Evaluate a `cmp`/`filter` operator against two values.
pub(crate) fn eval_op(op: &str, v1: &Value, v2: &Value) -> bool {
    let Some((kind, canon)) = classify(op) else {
        return false;
    };
    match kind {
        OpKind::Num => eval_num(canon, v1, v2),
        OpKind::List => eval_list(canon, v1, v2),
        OpKind::Str => eval_str(canon, v1, v2),
    }
}

/// Default existence check (no operator).
fn existence(v1: &Value) -> bool {
    match v1 {
        Value::Null => false,
        Value::List(v) => v.iter().any(|s| !s.trim().is_empty()),
        other => !other.to_compare_string().trim().is_empty(),
    }
}

/// Classic Libation `HasValue` conditional: true when the tagged field is non-empty.
pub(crate) fn has_value(v1: &Value) -> bool {
    match v1 {
        Value::Null => false,
        Value::List(v) => v.iter().any(|s| !s.trim().is_empty()),
        other => !other.to_compare_string().trim().is_empty(),
    }
}

fn eval_num(canon: &str, v1: &Value, v2: &Value) -> bool {
    let (Some(a), Some(b)) = (v1.to_int(), v2.to_int()) else {
        return false;
    };
    match canon {
        "eq" => a == b,
        "ne" => a != b,
        "ge" => a >= b,
        "gt" => a > b,
        "le" => a <= b,
        "lt" => a < b,
        _ => false,
    }
}

fn ci_eq(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn regex_match(pattern: &str, input: &str) -> bool {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map(|re| re.is_match(input))
        .unwrap_or(false)
}

fn check_item(canon: &str, a: &str, b: &str) -> bool {
    match canon {
        "eq" => ci_eq(a, b),
        "ne" => !ci_eq(a, b),
        "regex" => regex_match(b, a),
        "not_regex" => !regex_match(b, a),
        _ => false,
    }
}

fn eval_str(canon: &str, v1: &Value, v2: &Value) -> bool {
    if v1.is_null() || v2.is_null() {
        return false;
    }
    match (v1, v2) {
        (Value::List(e1), _) => {
            let b = v2.to_compare_string();
            e1.iter().any(|l| check_item(canon, l, &b))
        }
        (_, Value::List(e2)) => {
            let a = v1.to_compare_string();
            e2.iter().any(|r| check_item(canon, &a, r))
        }
        _ => check_item(canon, &v1.to_compare_string(), &v2.to_compare_string()),
    }
}

fn eval_list(canon: &str, v1: &Value, v2: &Value) -> bool {
    if v1.is_null() || v2.is_null() {
        return false;
    }
    let e1 = v1.to_enumerable();
    let e2 = v2.to_enumerable();
    list_check(canon, &e1, &e2)
}

fn contains_ci(haystack: &[String], needle: &str) -> bool {
    haystack.iter().any(|h| ci_eq(h, needle))
}

fn is_subset(a: &[String], b: &[String]) -> bool {
    a.iter().all(|l| contains_ci(b, l))
}

fn is_proper_subset(a: &[String], b: &[String]) -> bool {
    is_subset(a, b) && b.iter().any(|r| !contains_ci(a, r))
}

fn overlaps(a: &[String], b: &[String]) -> bool {
    a.iter().any(|l| contains_ci(b, l))
}

fn list_check(canon: &str, e1: &[String], e2: &[String]) -> bool {
    match canon {
        // e1 contains all of e2
        "contains" | "superset" => is_subset(e2, e1),
        "proper_superset" => is_proper_subset(e2, e1),
        "not_contains" => !is_subset(e2, e1),
        // e1 is contained in e2
        "in" | "subset" => is_subset(e1, e2),
        "proper_subset" => is_proper_subset(e1, e2),
        "not_in" => !is_subset(e1, e2),
        "overlaps" => overlaps(e1, e2),
        "disjoint" => !overlaps(e1, e2),
        "equals" => {
            let mut a: Vec<String> = e1.iter().map(|s| s.to_lowercase()).collect();
            let mut b: Vec<String> = e2.iter().map(|s| s.to_lowercase()).collect();
            a.sort();
            b.sort();
            a == b
        }
        _ => false,
    }
}

/// Parse a literal (`'quoted'`, `"quoted"`, or integer). Mirrors `TryGetLiteral`.
pub(crate) fn try_get_literal(value: &str) -> Option<Value> {
    let trimmed = value.trim();
    let bytes: Vec<char> = trimmed.chars().collect();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == '\'' || first == '"') && last == first {
            let inner: String = bytes[1..bytes.len() - 1].iter().collect();
            let doubled = format!("{first}{first}");
            let single = first.to_string();
            return Some(Value::Str(inner.replace(&doubled, &single)));
        }
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        return Some(Value::Int(i));
    }
    None
}
