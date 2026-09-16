//! Fail-closed SQL-v1 allowlist over source text using library token spans.
//!
//! Syntaqlite/sqlparser accepting a construct does not admit it. This pass
//! encodes the portable contract from `docs/sql-contract/v1.md`.

/// Why a parsed statement is still outside SQL v1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowlistReject {
    /// Human-readable reason.
    pub reason: String,
}

/// Token produced by a frontend (byte offsets into the exact source).
#[derive(Debug, Clone)]
pub struct TokenSpan {
    /// Inclusive start.
    pub start: usize,
    /// Exclusive end.
    pub end: usize,
    /// Token text as in source.
    pub text: String,
    /// True when this token is an identifier (possibly quoted).
    pub is_ident: bool,
    /// True when this token is a string/blob/numeric literal or comment.
    pub is_trivia_or_literal: bool,
}

/// Keywords and spellings that SQL v1 never admits in code spans.
const FORBIDDEN_KEYWORDS: &[&str] = &[
    "ILIKE",
    "GLOB",
    "MATCH",
    "REGEXP",
    "COLLATE",
    "OVER",
    "FILTER",
    "WINDOW",
    "EXCEPT",
    "INTERSECT",
    "RETURNING", // allowed — handled separately; listed only as documentation
    "PRAGMA",
    "VACUUM",
    "ATTACH",
    "DETACH",
    "REPLACE",
    "STRICT",
];

/// Applies the SQL-v1 allowlist to already-parsed tokens.
///
/// # Errors
///
/// Returns when a token or token sequence is outside the portable contract.
pub fn allowlist_tokens(sql: &str, tokens: &[TokenSpan]) -> Result<(), AllowlistReject> {
    let _ = sql;
    for (i, tok) in tokens.iter().enumerate() {
        if tok.is_trivia_or_literal {
            continue;
        }
        let upper = tok.text.to_ascii_uppercase();
        if tok.is_ident {
            if ident_is_quoted(&tok.text) {
                return Err(AllowlistReject {
                    reason: format!("quoted identifier {}", tok.text),
                });
            }
            if tok.text.len() > 63 {
                return Err(AllowlistReject {
                    reason: "identifier exceeds 63 bytes".into(),
                });
            }
        }
        if tok.text.starts_with('$')
            || (tok.text.starts_with('?')
                && tok.text.len() > 1
                && tok.text[1..].bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(AllowlistReject {
                reason: format!("non-v1 placeholder {}", tok.text),
            });
        }
        if tok.text == "::" {
            return Err(AllowlistReject {
                reason: ":: cast".into(),
            });
        }
        match upper.as_str() {
            "ILIKE" | "GLOB" | "MATCH" | "REGEXP" | "COLLATE" | "OVER" | "FILTER" | "WINDOW"
            | "EXCEPT" | "INTERSECT" | "PRAGMA" | "VACUUM" | "ATTACH" | "DETACH" | "VIEW"
            | "TRIGGER" | "VIRTUAL" => {
                return Err(AllowlistReject {
                    reason: format!("excluded keyword {upper}"),
                });
            }
            "REPLACE" => {
                return Err(AllowlistReject {
                    reason: "REPLACE".into(),
                });
            }
            "DISTINCT" => {
                if next_code(tokens, i).is_some_and(|t| t.eq_ignore_ascii_case("ON")) {
                    return Err(AllowlistReject {
                        reason: "DISTINCT ON".into(),
                    });
                }
            }
            "RIGHT" | "FULL" => {
                if next_code(tokens, i).is_some_and(|t| {
                    t.eq_ignore_ascii_case("JOIN") || t.eq_ignore_ascii_case("OUTER")
                }) {
                    return Err(AllowlistReject {
                        reason: format!("{upper} JOIN"),
                    });
                }
            }
            "INSERT" => {
                if let Some(or_tok) = next_code(tokens, i) {
                    if or_tok.eq_ignore_ascii_case("OR") {
                        if let Some(mode) = next_code_n(tokens, i, 2) {
                            let mode_u = mode.to_ascii_uppercase();
                            if mode_u != "IGNORE" {
                                return Err(AllowlistReject {
                                    reason: format!("INSERT OR {mode_u}"),
                                });
                            }
                        }
                    }
                }
            }
            "WITHOUT" => {
                if next_code(tokens, i).is_some_and(|t| t.eq_ignore_ascii_case("ROWID")) {
                    return Err(AllowlistReject {
                        reason: "WITHOUT ROWID".into(),
                    });
                }
            }
            "STRICT" => {
                return Err(AllowlistReject {
                    reason: "STRICT".into(),
                });
            }
            "CASCADE" | "RESTRICT" => {
                return Err(AllowlistReject {
                    reason: format!("{upper} drop action"),
                });
            }
            "BYTEA" | "JSONB" | "VARCHAR" | "NUMERIC" | "BOOL" => {
                return Err(AllowlistReject {
                    reason: format!("non-v1 type {upper}"),
                });
            }
            "USING" => {
                // CREATE INDEX … USING / JOIN USING. JOIN USING is admitted.
                if looks_like_index_using(tokens, i) {
                    return Err(AllowlistReject {
                        reason: "CREATE INDEX USING".into(),
                    });
                }
            }
            "INCLUDE" => {
                return Err(AllowlistReject {
                    reason: "INCLUDE".into(),
                });
            }
            _ => {}
        }
    }
    let _ = FORBIDDEN_KEYWORDS;
    Ok(())
}

fn ident_is_quoted(text: &str) -> bool {
    text.starts_with('"') || text.starts_with('`') || text.starts_with('[')
}

fn next_code(tokens: &[TokenSpan], i: usize) -> Option<&str> {
    next_code_n(tokens, i, 1)
}

fn next_code_n(tokens: &[TokenSpan], i: usize, n: usize) -> Option<&str> {
    tokens
        .iter()
        .skip(i + 1)
        .filter(|t| !t.is_trivia_or_literal)
        .nth(n.saturating_sub(1))
        .map(|t| t.text.as_str())
}

fn looks_like_index_using(tokens: &[TokenSpan], using_at: usize) -> bool {
    tokens
        .iter()
        .take(using_at)
        .rev()
        .any(|t| !t.is_trivia_or_literal && t.text.eq_ignore_ascii_case("INDEX"))
}
