//! Portable BookclerkSQL text domain and statement-boundary helpers.
//!
//! BookclerkSQL `TEXT` is Unicode scalar values representable on the UTF-8
//! wire, excluding U+0000. UTF-8 is the encoding, not a smaller repertoire.
//! `BYTES` / `BLOB` may contain zero bytes.

use sha2::{Digest, Sha256};

use crate::{PluginError, Result};

/// True when `text` contains U+0000.
#[must_use]
pub fn text_contains_nul(text: &str) -> bool {
    text.as_bytes().contains(&0)
}

/// Rejects BookclerkSQL `TEXT` that contains U+0000.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `text` contains NUL.
pub fn require_portable_text(text: &str) -> Result<()> {
    if text_contains_nul(text) {
        Err(PluginError::invalid_params(
            "BookclerkSQL TEXT cannot contain U+0000 (use BYTES/BLOB for binary)",
        ))
    } else {
        Ok(())
    }
}

/// Rejects U+0000 in every BookclerkSQL `TEXT` bind.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a [`crate::DbValue::Text`] bind
/// contains NUL.
pub fn require_portable_text_binds(parameters: &[crate::DbValue]) -> Result<()> {
    for (i, param) in parameters.iter().enumerate() {
        if let crate::DbValue::Text(text) = param {
            require_portable_text(text)
                .map_err(|err| PluginError::invalid_params(format!("parameter {i}: {err}")))?;
        }
    }
    Ok(())
}

/// Prefix of the sqlite-family LIKE→GLOB `replace(...)` wrap (before the pattern).
///
/// Must stay identical to the GLOB wrap in `bookclerk-db-exec`.
pub const LIKE_GLOB_WRAP_PREFIX: &str = "replace(replace(replace(replace(replace((";

/// Suffix of the sqlite-family LIKE→GLOB `replace(...)` wrap (after the pattern).
pub const LIKE_GLOB_WRAP_SUFFIX: &str =
    "), '[', '[[]'), '*', '[*]'), '?', '[?]'), '%', '*'), '_', '?')";

/// Bytes added around a LIKE pattern by [`LIKE_GLOB_WRAP_PREFIX`] /
/// [`LIKE_GLOB_WRAP_SUFFIX`] (the pattern itself is unchanged).
pub const LIKE_GLOB_PATTERN_WRAP_BYTES: usize =
    LIKE_GLOB_WRAP_PREFIX.len() + LIKE_GLOB_WRAP_SUFFIX.len();

/// Extra byte when `LIKE` is immediately followed by its pattern (no trivia):
/// the rewrite emits `GLOB ` (5) in place of `LIKE` (4).
pub const LIKE_TO_GLOB_KEYWORD_EXTRA: usize = 1;

/// Worst-case extra bytes per `LIKE` / `NOT LIKE` after sqlite-family GLOB wrapping.
pub const LIKE_GLOB_REWRITE_OVERHEAD: usize =
    LIKE_GLOB_PATTERN_WRAP_BYTES + LIKE_TO_GLOB_KEYWORD_EXTRA;

/// Extra bytes for one `/` or `%` rewritten to `NULLIF(operand, 0)`.
pub const SQLITE_FAMILY_DIV_MOD_NULLIF_EXTRA: usize = 11;

/// Conservative extra bytes for one `INSERT OR IGNORE` unique-conflict rewrite
/// (conflict clause plus optional `SELECT` source wrap).
pub const SQLITE_FAMILY_INSERT_OR_IGNORE_MAX_EXTRA: usize = 256;

/// Upper bound on sqlite-family mechanical lowering length for a canonical
/// statement of `canonical_len` bytes (`div`/`mod` NULLIF, `INSERT OR IGNORE`,
/// LIKE→GLOB). Does not include proof-directed INTEGER overflow wraps.
///
/// LIKE→GLOB is the densest per-byte expansion among mechanical rewrites
/// (`LIKE` is 4 bytes; overhead is [`LIKE_GLOB_REWRITE_OVERHEAD`]). The count
/// `canonical_len / 4` is the maximum number of `LIKE` keywords that can appear
/// in `canonical_len` bytes — not an expansion-factor guess. Remaining bytes
/// after packing as many `LIKE` keywords as possible are charged as `/` `%`
/// NULLIF wraps (1-byte operators).
///
/// Proof-directed INTEGER overflow CASE wraps are applied from typed proofs
/// before this mechanical bound; they are not a function of payload length
/// alone.
#[must_use]
pub const fn sqlite_family_like_divmod_insert_upper_bound(canonical_len: usize) -> usize {
    const {
        assert!(
            LIKE_GLOB_REWRITE_OVERHEAD >= 4 * SQLITE_FAMILY_DIV_MOD_NULLIF_EXTRA,
            "LIKE→GLOB must remain the densest mechanical per-byte expansion"
        );
    }
    let like_extra = (canonical_len / 4).saturating_mul(LIKE_GLOB_REWRITE_OVERHEAD);
    let remainder_divmod = (canonical_len % 4).saturating_mul(SQLITE_FAMILY_DIV_MOD_NULLIF_EXTRA);
    canonical_len
        .saturating_add(like_extra)
        .saturating_add(remainder_divmod)
        .saturating_add(SQLITE_FAMILY_INSERT_OR_IGNORE_MAX_EXTRA)
}

/// Mechanical sqlite-family length bound for one canonical statement, counting
/// actual `LIKE` occurrences, `/` `%` operators in code spans, and at most one
/// `INSERT OR IGNORE` rewrite.
///
/// Always `<=` [`sqlite_family_like_divmod_insert_upper_bound`]`(sql.len())`
/// for well-formed packs: the length formula packs the densest rewrite into
/// every 4-byte window, while this preflight charges only constructs that
/// lowering actually rewrites. INTEGER overflow wraps are applied from typed
/// proofs *before* this mechanical bound.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when quotes or block comments are
/// unterminated.
pub fn sqlite_family_mechanical_len_upper_bound(sql: &str) -> Result<usize> {
    let likes = like_pattern_sources(sql)?.len();
    let div_mods = count_div_mod_operators(sql)?;
    let insert_extra = insert_or_ignore_rewrite_extra(sql)?;
    Ok(sql
        .len()
        .saturating_add(likes.saturating_mul(LIKE_GLOB_REWRITE_OVERHEAD))
        .saturating_add(div_mods.saturating_mul(SQLITE_FAMILY_DIV_MOD_NULLIF_EXTRA))
        .saturating_add(insert_extra))
}

/// UTF-8 byte length of a sqlite-family GLOB pattern after Bookclerk LIKE→GLOB
/// metacharacter escaping (`[` `*` `?` become three bytes).
///
/// D1's physical GLOB/LIKE pattern cap is 50 bytes. Adapters that perform this
/// expansion must advertise [`super::DbCapabilities::max_pattern_bytes`] small
/// enough that the expanded pattern still fits.
#[must_use]
pub fn glob_expanded_like_pattern_bytes(like_pattern: &str) -> usize {
    let mut n = 0usize;
    for c in like_pattern.chars() {
        n = n.saturating_add(match c {
            '[' | '*' | '?' => 3,
            _ => c.len_utf8(),
        });
    }
    n
}

/// Conservative portable LIKE pattern cap for D1 while LIKE lowers to GLOB.
///
/// Cloudflare D1 allows 50 bytes in a LIKE/GLOB pattern. Worst-case expansion
/// of `[` / `*` / `?` is 3×, so `floor(50/3) = 16`.
pub const D1_PORTABLE_LIKE_PATTERN_BYTES: u32 = 16;

/// Physical D1 LIKE/GLOB pattern cap in bytes.
pub const D1_PHYSICAL_LIKE_GLOB_PATTERN_BYTES: u32 = 50;

/// Length-prefixed SHA-256 of an ordered canonical statement list.
///
/// Statement boundaries are part of the digest: joining with `;` then hashing
/// would collide with literals such as `DEFAULT 'a;b'`.
#[must_use]
pub fn canonical_statements_checksum(statements: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bookclerk-sql-stmts-v1\0");
    for sql in statements {
        let len = u64::try_from(sql.len()).unwrap_or(u64::MAX);
        hasher.update(len.to_le_bytes());
        hasher.update(sql.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Splits a BookclerkSQL pack into statements using the SQL-v1 lexer.
///
/// Semicolons inside string literals, quoted identifiers, line comments, and
/// block comments do not end a statement. Empty fragments are dropped.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when quotes or comments are
/// unterminated, or when a fragment is not a single SQL-v1 statement.
pub fn sql_v1_pack_statements(sql: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < sql.len() {
        i = skip_trivia(sql, i)?;
        if i >= sql.len() {
            break;
        }
        let start = i;
        let (end, saw_semi) = statement_span(sql, start)?;
        let raw = sql.get(start..end).unwrap_or("");
        let stmt = raw.trim();
        if !stmt.is_empty() {
            require_portable_text(stmt)?;
            out.push(stmt.to_string());
        }
        i = end;
        if saw_semi {
            i += 1;
        }
    }
    Ok(out)
}

/// Walks one statement starting at `start` (trivia already skipped).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a string, quoted identifier,
/// or block comment is unterminated.
fn statement_span(sql: &str, start: usize) -> Result<(usize, bool)> {
    let bytes = sql.as_bytes();
    let mut i = start;
    let mut depth = 0usize;
    while i < bytes.len() {
        i = skip_trivia(sql, i)?;
        if i >= bytes.len() {
            break;
        }
        let c = bytes[i];
        match c {
            b'\'' => {
                i = skip_sql_string(sql, i)?;
            }
            b'"' | b'`' => {
                i = skip_quoted(sql, i, c)?;
            }
            b'[' => {
                i = skip_quoted(sql, i, b']')?;
            }
            b'(' => {
                depth = depth.saturating_add(1);
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b';' if depth == 0 => return Ok((i, true)),
            _ => {
                i += 1;
            }
        }
    }
    Ok((sql.len(), false))
}

/// Skips whitespace, `--` line comments, and `/* */` block comments.
///
/// Line comments may run to EOF. Block comments must close with `*/`.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a `/*` block comment is not
/// terminated before EOF.
fn skip_trivia(sql: &str, mut i: usize) -> Result<usize> {
    let bytes = sql.as_bytes();
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) == Some(&b'-') && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            loop {
                if i + 1 >= bytes.len() {
                    return Err(PluginError::invalid_params(
                        "unterminated block comment in canonical pack",
                    ));
                }
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        break;
    }
    Ok(i)
}

/// Skips a `'…'` SQL string, honoring doubled quotes.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `start` is not `'`, or the
/// string is unterminated.
fn skip_sql_string(sql: &str, start: usize) -> Result<usize> {
    let bytes = sql.as_bytes();
    if bytes.get(start) != Some(&b'\'') {
        return Err(PluginError::invalid_params("expected SQL string"));
    }
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Ok(i + 1);
        }
        i += 1;
    }
    Err(PluginError::invalid_params(
        "unterminated SQL string in canonical pack",
    ))
}

/// Skips a quoted identifier delimited by `end` (doubled for `"` / `` ` ``).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the quoted identifier is
/// unterminated.
fn skip_quoted(sql: &str, start: usize, end: u8) -> Result<usize> {
    let bytes = sql.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == end {
            if end != b']' && bytes.get(i + 1) == Some(&end) {
                i += 2;
                continue;
            }
            return Ok(i + 1);
        }
        i += 1;
    }
    Err(PluginError::invalid_params(
        "unterminated quoted identifier in canonical pack",
    ))
}

/// Proven LIKE pattern sources in one canonical statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LikePatternSrc {
    /// `'literal'` (possibly concatenated with `||`).
    Literal(String),
    /// Positional `?` bind (0-based bind index in this statement).
    Bind(usize),
    /// Pattern expression whose length cannot be proven.
    Unproven,
    /// SQL `NULL` (no pattern bytes).
    Null,
}

/// Collects LIKE / NOT LIKE pattern sources with proven lengths when possible.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a string, quoted identifier,
/// or block comment is unterminated.
pub fn like_pattern_sources(sql: &str) -> Result<Vec<LikePatternSrc>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut bind_i = 0usize;
    let bytes = sql.as_bytes();
    while i < bytes.len() {
        i = skip_trivia(sql, i)?;
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'\'' {
            i = skip_sql_string(sql, i)?;
            continue;
        }
        if bytes[i] == b'"' || bytes[i] == b'`' {
            i = skip_quoted(sql, i, bytes[i])?;
            continue;
        }
        if bytes[i] == b'[' {
            i = skip_quoted(sql, i, b']')?;
            continue;
        }
        if bytes[i] == b'?' {
            bind_i += 1;
            i += 1;
            continue;
        }
        if keyword_at(sql, i, "LIKE") {
            let after = skip_trivia(sql, i + 4)?;
            out.push(parse_like_pattern(sql, after, bind_i)?.0);
            i = after;
            continue;
        }
        i += 1;
    }
    Ok(out)
}

/// Parses the LIKE pattern starting at `start`.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when trivia or a string in the
/// pattern is unterminated.
fn parse_like_pattern(
    sql: &str,
    start: usize,
    binds_before: usize,
) -> Result<(LikePatternSrc, usize)> {
    let mut i = skip_trivia(sql, start)?;
    let bytes = sql.as_bytes();
    let mut literal = String::new();
    let mut saw_literal = false;
    let mut bind = None;
    loop {
        i = skip_trivia(sql, i)?;
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'\'' {
            let Ok(end) = skip_sql_string(sql, i) else {
                return Ok((LikePatternSrc::Unproven, i));
            };
            let inner = unquote_sql_string(&sql[i..end]);
            literal.push_str(&inner);
            saw_literal = true;
            i = skip_trivia(sql, end)?;
            if sql.get(i..).is_some_and(|s| s.starts_with("||")) {
                i += 2;
                continue;
            }
            break;
        }
        if bytes[i] == b'?' {
            bind = Some(binds_before);
            i += 1;
            break;
        }
        if keyword_at(sql, i, "NULL") {
            if saw_literal || bind.is_some() {
                return Ok((LikePatternSrc::Unproven, i));
            }
            return Ok((LikePatternSrc::Null, i + 4));
        }
        return Ok((LikePatternSrc::Unproven, i));
    }
    if let Some(b) = bind {
        if saw_literal {
            return Ok((LikePatternSrc::Unproven, i));
        }
        return Ok((LikePatternSrc::Bind(b), i));
    }
    if saw_literal {
        Ok((LikePatternSrc::Literal(literal), i))
    } else {
        Ok((LikePatternSrc::Unproven, i))
    }
}

/// Unquotes a `'…'` SQL string (doubled quotes become one quote).
fn unquote_sql_string(quoted: &str) -> String {
    let bytes = quoted.as_bytes();
    if bytes.first() != Some(&b'\'') || bytes.last() != Some(&b'\'') || bytes.len() < 2 {
        return String::new();
    }
    let inner = &quoted[1..quoted.len() - 1];
    inner.replace("''", "'")
}

/// True when `sql[i..]` is keyword `kw` with identifier boundaries.
fn keyword_at(sql: &str, i: usize, kw: &str) -> bool {
    if !sql.is_char_boundary(i) {
        return false;
    }
    let n = kw.len();
    let Some(end) = i.checked_add(n) else {
        return false;
    };
    if end > sql.len() || !sql.is_char_boundary(end) {
        return false;
    }
    if !sql[i..end].eq_ignore_ascii_case(kw) {
        return false;
    }
    let bytes = sql.as_bytes();
    let before_ok = i == 0 || !is_ident_cont(bytes[i - 1]);
    let after = bytes.get(end).copied().unwrap_or(b' ');
    before_ok && !is_ident_cont(after)
}

/// True when `c` may continue an unquoted identifier.
const fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Counts `/` and `%` operators in code spans (strings, identifiers, and
/// comments are not operators).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when quotes or block comments are
/// unterminated.
fn count_div_mod_operators(sql: &str) -> Result<usize> {
    let mut n = 0usize;
    let mut i = 0usize;
    let bytes = sql.as_bytes();
    while i < bytes.len() {
        i = skip_trivia(sql, i)?;
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'\'' => i = skip_sql_string(sql, i)?,
            b'"' | b'`' => i = skip_quoted(sql, i, bytes[i])?,
            b'[' => i = skip_quoted(sql, i, b']')?,
            b'/' | b'%' => {
                n = n.saturating_add(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(n)
}

/// [`SQLITE_FAMILY_INSERT_OR_IGNORE_MAX_EXTRA`] when the statement is
/// `INSERT OR IGNORE`, otherwise `0`.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a leading block comment is
/// unterminated.
fn insert_or_ignore_rewrite_extra(sql: &str) -> Result<usize> {
    let mut i = skip_trivia(sql, 0)?;
    if !keyword_at(sql, i, "INSERT") {
        return Ok(0);
    }
    i = skip_trivia(sql, i + "INSERT".len())?;
    if !keyword_at(sql, i, "OR") {
        return Ok(0);
    }
    i = skip_trivia(sql, i + "OR".len())?;
    if keyword_at(sql, i, "IGNORE") {
        Ok(SQLITE_FAMILY_INSERT_OR_IGNORE_MAX_EXTRA)
    } else {
        Ok(0)
    }
}

/// One helper call found in canonical SQL (code spans only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlFnCall {
    /// Folded helper name (`json_object`, `replace`, …).
    pub name: String,
    /// Top-level argument count (`json_object('k', 1)` is 2).
    pub arg_count: usize,
}

/// Helpers adapters may nest so each physical call stays inside
/// [`crate::DbCapabilities::max_function_args`].
///
/// `json_object` is not chunkable: portable SQL-v1 already caps it at
/// [`crate::SQL_V1_JSON_OBJECT_MAX_ARGS`] (D1's physical limit). Nesting via
/// `json_patch` is not equivalent (JSON Merge Patch deletes nulls).
#[must_use]
pub fn sql_v1_helper_is_chunkable(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "min" | "max" | "coalesce"
    )
}

/// Collects helper calls in code spans (strings/comments are skipped).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a string, quoted identifier,
/// or block comment is unterminated.
pub fn sql_v1_function_calls(sql: &str) -> Result<Vec<SqlFnCall>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let bytes = sql.as_bytes();
    while i < bytes.len() {
        i = skip_trivia(sql, i)?;
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'\'' => {
                i = skip_sql_string(sql, i)?;
                continue;
            }
            b'"' | b'`' => {
                i = skip_quoted(sql, i, bytes[i])?;
                continue;
            }
            b'[' => {
                i = skip_quoted(sql, i, b']')?;
                continue;
            }
            c if is_ident_start(c) => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                let name = sql[start..i].to_ascii_lowercase();
                let j = skip_trivia(sql, i)?;
                if bytes.get(j) == Some(&b'(') && !matches!(name.as_str(), "cast" | "case") {
                    if let Some((n, end)) = count_call_args(sql, j)? {
                        out.push(SqlFnCall { name, arg_count: n });
                        i = end;
                        continue;
                    }
                }
            }
            _ => i += 1,
        }
    }
    Ok(out)
}

/// True when `c` may start an unquoted identifier.
fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

/// Counts top-level arguments of a call whose `(` is at `open`.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a string, quoted identifier,
/// or block comment inside the argument list is unterminated.
fn count_call_args(sql: &str, open: usize) -> Result<Option<(usize, usize)>> {
    let bytes = sql.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return Ok(None);
    }
    let mut i = open + 1;
    let mut depth = 0usize;
    let mut n = 0usize;
    let mut saw_atom = false;
    while i < bytes.len() {
        i = skip_trivia(sql, i)?;
        if i >= bytes.len() {
            return Ok(None);
        }
        match bytes[i] {
            b'\'' => {
                i = skip_sql_string(sql, i)?;
                saw_atom = true;
            }
            b'"' | b'`' => {
                i = skip_quoted(sql, i, bytes[i])?;
                saw_atom = true;
            }
            b'[' => {
                i = skip_quoted(sql, i, b']')?;
                saw_atom = true;
            }
            b'(' => {
                depth = depth.saturating_add(1);
                saw_atom = true;
                i += 1;
            }
            b')' if depth == 0 => {
                if saw_atom {
                    n = n.saturating_add(1);
                }
                return Ok(Some((n, i + 1)));
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b',' if depth == 0 => {
                n = n.saturating_add(1);
                saw_atom = false;
                i += 1;
            }
            _ => {
                saw_atom = true;
                i += 1;
            }
        }
    }
    Ok(None)
}

/// Enforces [`crate::DbCapabilities::max_function_args`] on non-chunkable helpers.
///
/// `min` / `max` / `coalesce` are omitted: adapters nest them so each physical
/// call stays inside the advertised cap. `json_object` is enforced here: the
/// portable maximum is already [`crate::SQL_V1_JSON_OBJECT_MAX_ARGS`].
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a non-chunkable call exceeds
/// the cap or the cap is unspecified.
pub fn require_function_args_within(sql: &str, max_function_args: u32) -> Result<()> {
    if max_function_args == 0 {
        return Err(PluginError::invalid_params(
            "database guest maxFunctionArgs is unspecified",
        ));
    }
    let cap = usize::try_from(max_function_args).unwrap_or(usize::MAX);
    for call in sql_v1_function_calls(sql)? {
        if sql_v1_helper_is_chunkable(&call.name) {
            continue;
        }
        if call.arg_count > cap {
            return Err(PluginError::invalid_params(format!(
                "function {} has {} arguments; guest maxFunctionArgs is {max_function_args}",
                call.name, call.arg_count
            )));
        }
    }
    Ok(())
}

/// Enforces [`crate::DbCapabilities::max_pattern_bytes`] on LIKE patterns.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a proven pattern exceeds the
/// cap or a LIKE pattern length cannot be proven.
pub fn require_like_patterns_within(
    sql: &str,
    parameters: &[crate::DbValue],
    max_pattern_bytes: u32,
) -> Result<()> {
    if max_pattern_bytes == 0 {
        return Err(PluginError::invalid_params(
            "database guest maxPatternBytes is unspecified",
        ));
    }
    let cap = usize::try_from(max_pattern_bytes).unwrap_or(usize::MAX);
    for src in like_pattern_sources(sql)? {
        match src {
            LikePatternSrc::Literal(pat) => {
                require_portable_text(&pat)?;
                if pat.len() > cap {
                    return Err(PluginError::invalid_params(format!(
                        "LIKE pattern is {} bytes; guest maxPatternBytes is {max_pattern_bytes}",
                        pat.len()
                    )));
                }
            }
            LikePatternSrc::Bind(idx) => match parameters.get(idx) {
                Some(crate::DbValue::Text(pat)) => {
                    require_portable_text(pat)?;
                    if pat.len() > cap {
                        return Err(PluginError::invalid_params(format!(
                            "LIKE bind {idx} is {} bytes; guest maxPatternBytes is {max_pattern_bytes}",
                            pat.len()
                        )));
                    }
                }
                Some(crate::DbValue::Null(_)) => {}
                Some(_) => {
                    return Err(PluginError::invalid_params(format!(
                        "LIKE bind {idx} must be TEXT"
                    )));
                }
                None => {
                    return Err(PluginError::invalid_params(format!(
                        "LIKE bind {idx} is missing"
                    )));
                }
            },
            LikePatternSrc::Null => {}
            LikePatternSrc::Unproven => {
                return Err(PluginError::invalid_params(
                    "LIKE pattern length is not proven to fit maxPatternBytes",
                ));
            }
        }
    }
    Ok(())
}

/// Grammar-aware BookclerkSQL samples for fuzz corpora and differential tests.
///
/// Statements stay inside the SQL-v1 grammar (canonical `?`, `LIKE`,
/// `INSERT OR IGNORE`). TEXT literals are portable UTF-8 without U+0000.
/// LIKE patterns stay within [`D1_PORTABLE_LIKE_PATTERN_BYTES`].
#[must_use]
pub fn admitted_bookclerk_sql_samples(seed: u64, count: usize) -> Vec<String> {
    let mut rng = SplitMix64::new(seed | 1);
    let texts = [
        "ok",
        "café",
        "日本語",
        "a;b",
        "it's",
        "emoji😀",
        "",
        "line\nbreak",
    ];
    let like_pats = ["a%", "_b", "x", "ab", "%", ""];
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let sql = match rng.bounded(8) {
            0 => format!("SELECT {} AS n", rng.bounded(10_000) as i64 - 5000),
            1 => {
                let t = sql_quote(texts[rng.bounded(texts.len() as u64) as usize]);
                format!("SELECT {t} AS t")
            }
            2 => "SELECT ? AS v".to_string(),
            3 => format!("SELECT {} + {} AS n", rng.bounded(100), rng.bounded(100)),
            4 => {
                let t = sql_quote(texts[rng.bounded(texts.len() as u64) as usize]);
                let p = sql_quote(like_pats[rng.bounded(like_pats.len() as u64) as usize]);
                format!("SELECT CASE WHEN {t} LIKE {p} THEN 1 ELSE 0 END AS m")
            }
            5 => "SELECT CASE WHEN 'x' LIKE NULL THEN 1 ELSE 0 END AS m".to_string(),
            6 => {
                let id = format!("s{i:04}");
                format!(
                    "INSERT OR IGNORE INTO db_serialization_slots (slot_key, bump) VALUES ('{id}', {})",
                    rng.bounded(8)
                )
            }
            _ => {
                let id = format!("s{i:04}");
                format!(
                    "SELECT slot_key FROM db_serialization_slots WHERE slot_key = '{id}' ORDER BY slot_key"
                )
            }
        };
        out.push(sql);
    }
    // Length/extract keep SQLite/Postgres differentials portable (JSON
    // spacing and duplicate-key winner are engine-specific). Postgres
    // lowering casts `json_build_object` to TEXT so `length` is legal. The
    // calls still exercise 2-arg null retention and 32-arg (16-pair) arity.
    out.push(
        "SELECT CASE WHEN length(json_object('a', 1, 'b', NULL)) \
             > length(json_object('a', 1)) THEN 1 ELSE 0 END"
            .into(),
    );
    let pairs: Vec<String> = (0..16).map(|i| format!("'k{i:02}', 'v{i:02}'")).collect();
    out.push(format!(
        "SELECT json_extract(json_object({}), '$.k15')",
        pairs.join(", ")
    ));
    // Unaliased pair: Postgres names both `?column?`; the adapter uniquifies
    // those engine labels so this stays a / % semantics sample, not a
    // duplicate-name check (`SELECT x, x` still fails closed).
    out.push("SELECT 1 / NULLIF(0, 1), 1 % NULLIF(0, 1)".into());
    out.push("SELECT 10 / NULLIF(2, 0)".into());
    out
}

/// Doubles single quotes for a SQL string literal.
fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// SplitMix64 for deterministic sample generation (no extra crate).
struct SplitMix64 {
    /// Generator state.
    state: u64,
}

impl SplitMix64 {
    /// Seeds the generator.
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next 64-bit value.
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..n` (`n == 0` is treated as 1).
    fn bounded(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn pack_statements_preserve_semicolons_in_literals_and_comments() {
        let sql = r#"
            CREATE TABLE t (
                x TEXT NOT NULL DEFAULT 'a;b',
                y TEXT CHECK (x <> ';')
            );
            -- trailing; comment
            /* block ; comment */
            CREATE INDEX idx ON t (x);
        "#;
        let stmts = sql_v1_pack_statements(sql).expect("pack");
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert!(stmts[0].contains("DEFAULT 'a;b'"), "{}", stmts[0]);
        assert!(stmts[0].contains("CHECK (x <> ';')"), "{}", stmts[0]);
        assert!(stmts[1].to_ascii_lowercase().contains("create index"));
        let again = sql_v1_pack_statements(&stmts.join(";\n")).expect("repack");
        assert_eq!(again, stmts);
        assert_eq!(
            canonical_statements_checksum(&stmts.iter().map(String::as_str).collect::<Vec<_>>()),
            canonical_statements_checksum(&again.iter().map(String::as_str).collect::<Vec<_>>())
        );
    }

    #[test]
    fn escaped_quotes_around_semicolons() {
        let sql = "CREATE TABLE t (x TEXT DEFAULT 'a'';b')";
        let stmts = sql_v1_pack_statements(sql).expect("pack");
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("''"));
    }

    #[test]
    fn nul_is_rejected_in_text() {
        assert!(require_portable_text("ok").is_ok());
        assert!(require_portable_text("a\0b").is_err());
        assert!(!text_contains_nul("🎵"));
    }

    #[test]
    fn glob_expansion_is_three_for_metacharacters() {
        assert_eq!(glob_expanded_like_pattern_bytes("abc"), 3);
        assert_eq!(glob_expanded_like_pattern_bytes("["), 3);
        assert_eq!(
            glob_expanded_like_pattern_bytes(&"[".repeat(D1_PORTABLE_LIKE_PATTERN_BYTES as usize)),
            (D1_PORTABLE_LIKE_PATTERN_BYTES as usize) * 3
        );
        assert!(
            glob_expanded_like_pattern_bytes(&"[".repeat(D1_PORTABLE_LIKE_PATTERN_BYTES as usize))
                <= D1_PHYSICAL_LIKE_GLOB_PATTERN_BYTES as usize
        );
    }

    #[test]
    fn like_literal_and_bind_sources() {
        let srcs = like_pattern_sources("SELECT * FROM t WHERE a LIKE 'ab%' AND b LIKE ?")
            .expect("like sources");
        assert_eq!(
            srcs,
            vec![
                LikePatternSrc::Literal("ab%".into()),
                LikePatternSrc::Bind(0)
            ]
        );
    }

    #[test]
    fn like_null_is_proven_and_fits_any_pattern_cap() {
        let srcs = like_pattern_sources(
            "SELECT CASE WHEN 'A' LIKE NULL THEN 1 ELSE 0 END AS c7, \
             CASE WHEN body LIKE ? THEN 1 ELSE 0 END AS c8 FROM liked",
        )
        .expect("like sources");
        assert_eq!(
            srcs,
            vec![LikePatternSrc::Null, LikePatternSrc::Bind(0)],
            "{srcs:?}"
        );
        require_like_patterns_within(
            "SELECT CASE WHEN 'A' LIKE NULL THEN 1 ELSE 0 END AS c0 FROM t",
            &[],
            D1_PORTABLE_LIKE_PATTERN_BYTES,
        )
        .expect("LIKE NULL has no pattern bytes");
        require_like_patterns_within(
            "SELECT CASE WHEN 'A' LIKE 'a' THEN 1 ELSE 0 END AS c0, \
             CASE WHEN 'A' LIKE NULL THEN 1 ELSE 0 END AS c7, \
             CASE WHEN body LIKE ? THEN 1 ELSE 0 END AS c8 FROM liked",
            &[crate::DbValue::Text("A".into())],
            D1_PORTABLE_LIKE_PATTERN_BYTES,
        )
        .expect("portable LIKE vector patterns");
    }

    #[test]
    fn portable_text_accepts_unicode_and_rejects_nul() {
        for sample in [
            "",
            "ascii",
            "café",
            "cafe\u{0301}",
            "汉语",
            "日本語",
            "한국어",
            "𠀀",
            "مرحبا",
            "שלום",
            "नमस्ते",
            "สวัสดี",
            "γειά",
            "привет",
            "🎵",
            "\u{1F980}",
            "a\u{0301}",
        ] {
            require_portable_text(sample).expect(sample);
        }
        assert_ne!("café".as_bytes(), "cafe\u{0301}".as_bytes());
        assert!(require_portable_text("a\0b").is_err());
        require_portable_text_binds(&[crate::DbValue::Bytes(vec![0])]).expect("BLOB NUL");
        assert!(require_portable_text_binds(&[crate::DbValue::Text("a\0".into())]).is_err());
    }

    #[test]
    fn checksum_distinguishes_semicolon_in_literal() {
        let a = canonical_statements_checksum(&["CREATE TABLE t (x TEXT DEFAULT 'a;b')"]);
        let b = canonical_statements_checksum(&["CREATE TABLE t (x TEXT DEFAULT 'a", "b')"]);
        assert_ne!(a, b);
    }

    #[test]
    fn function_call_arity_and_chunkable_helpers() {
        let calls = sql_v1_function_calls(
            "SELECT replace(a, 'x', 'y'), json_object('k', 1, 'm', 2), min(1, 2, 3) FROM t",
        )
        .expect("function calls");
        assert!(
            calls
                .iter()
                .any(|c| c.name == "replace" && c.arg_count == 3),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.name == "json_object" && c.arg_count == 4),
            "{calls:?}"
        );
        require_function_args_within("SELECT replace(a, 'x', 'y')", 3).expect("N");
        let err = require_function_args_within("SELECT replace(a, 'x', 'y')", 2).unwrap_err();
        assert!(err.to_string().contains("maxFunctionArgs"), "{err}");
        let err =
            require_function_args_within("SELECT json_object('a', 1, 'b', 2, 'c', 3, 'd', 4)", 2)
                .unwrap_err();
        assert!(
            err.to_string().contains("maxFunctionArgs"),
            "json_object is not adapter-chunked: {err}"
        );
        require_function_args_within("SELECT json_object('a', 1, 'b', 2, 'c', 3, 'd', 4)", 8)
            .expect("4 args under cap 8");
        let max_pairs: Vec<String> = (0..16).map(|i| format!("'{i}', {i}")).collect();
        let max_sql = format!("SELECT json_object({})", max_pairs.join(", "));
        require_function_args_within(&max_sql, crate::D1_MAX_FUNCTION_ARGS)
            .expect("32-arg json_object matches D1 cap");
        let err = require_function_args_within(&max_sql, 31).unwrap_err();
        assert!(err.to_string().contains("maxFunctionArgs"), "{err}");
        require_function_args_within("SELECT min(1, 2, 3)", 2).expect("min stays adapter-chunked");
    }

    #[test]
    fn admitted_samples_pack_and_pass_grammar() {
        for sql in admitted_bookclerk_sql_samples(42, 32) {
            sql_v1_pack_statements(&sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
            crate::validate_sql_v1_grammar(&sql, false)
                .unwrap_or_else(|err| panic!("{sql}: {err}"));
            require_portable_text(&sql).expect("sample SQL is portable TEXT");
            require_like_patterns_within(&sql, &[], D1_PORTABLE_LIKE_PATTERN_BYTES)
                .unwrap_or_else(|err| panic!("{sql}: {err}"));
        }
    }

    #[test]
    fn fuzz_corpus_sql_parse_does_not_panic() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus/sql_parse");
        for entry in std::fs::read_dir(&dir).expect("fuzz corpus") {
            let path = entry.expect("entry").path();
            if !path.is_file() {
                continue;
            }
            let sql = std::fs::read_to_string(&path).expect("read");
            let _ = sql_v1_pack_statements(&sql);
            let _ = crate::validate_sql_v1_grammar(&sql, false);
            let _ = require_portable_text(&sql);
            let _ = crate::desugar_canonical_sql(&sql);
            let _ = like_pattern_sources(&sql);
            let _ = require_like_patterns_within(&sql, &[], D1_PORTABLE_LIKE_PATTERN_BYTES);
            let _ = require_function_args_within(&sql, 32);
        }
    }

    #[test]
    fn pack_rejects_unterminated_block_comment_string_and_ident() {
        let block = sql_v1_pack_statements("SELECT 1 /* unterminated").unwrap_err();
        assert!(
            block.to_string().contains("unterminated block comment"),
            "{block}"
        );
        let nested = sql_v1_pack_statements("SELECT 1; /* still open").unwrap_err();
        assert!(
            nested.to_string().contains("unterminated block comment"),
            "{nested}"
        );
        let string = sql_v1_pack_statements("SELECT 'oops").unwrap_err();
        assert!(
            string.to_string().contains("unterminated SQL string"),
            "{string}"
        );
        let ident = sql_v1_pack_statements("SELECT \"oops").unwrap_err();
        assert!(
            ident.to_string().contains("unterminated quoted identifier"),
            "{ident}"
        );
        let tick = sql_v1_pack_statements("SELECT `oops").unwrap_err();
        assert!(
            tick.to_string().contains("unterminated quoted identifier"),
            "{tick}"
        );
        let bracket = sql_v1_pack_statements("SELECT [oops").unwrap_err();
        assert!(
            bracket
                .to_string()
                .contains("unterminated quoted identifier"),
            "{bracket}"
        );
        sql_v1_pack_statements("SELECT 1 /* ok */").expect("terminated block");
        sql_v1_pack_statements("SELECT 1 -- to eof").expect("line comment to EOF");
        sql_v1_pack_statements("SELECT 1 /* foo ; bar */").expect("semicolon in block comment");
        sql_v1_pack_statements("SELECT 1 -- foo ; bar").expect("semicolon in line comment");
        let nested_open = sql_v1_pack_statements("SELECT 1 /* /* still open").unwrap_err();
        assert!(
            nested_open
                .to_string()
                .contains("unterminated block comment"),
            "{nested_open}"
        );
        sql_v1_pack_statements("SELECT 1 /* /* nested close */")
            .expect("SQL block comments are not nested; first */ terminates");
        sql_v1_pack_statements("SELECT '/* not a comment' /* real */ 'x'")
            .expect("comment markers inside strings");
        sql_v1_pack_statements("SELECT 1 /* 'unterminated string in comment */")
            .expect("quotes inside terminated comments");
        sql_v1_pack_statements("SELECT /* ' */ 1").expect("quote inside block comment");
        let odd_quote = sql_v1_pack_statements("SELECT 'a''b").unwrap_err();
        assert!(
            odd_quote.to_string().contains("unterminated SQL string"),
            "{odd_quote}"
        );
        require_like_patterns_within("SELECT 1 /* ", &[], D1_PORTABLE_LIKE_PATTERN_BYTES)
            .expect_err("LIKE scan fails closed on unterminated comment");
        require_function_args_within("SELECT replace(a, 'x', 'y') /* ", 8)
            .expect_err("function scan fails closed on unterminated comment");
    }

    #[test]
    fn mechanical_preflight_charges_likes_not_length_guess() {
        let sql = "SELECT 1 WHERE x LIKE'a' OR x LIKE'b'";
        let bound = sqlite_family_mechanical_len_upper_bound(sql).expect("preflight");
        assert_eq!(bound, sql.len() + 2 * LIKE_GLOB_REWRITE_OVERHEAD, "{bound}");
        assert!(bound <= sqlite_family_like_divmod_insert_upper_bound(sql.len()));
        let with_div = "SELECT 1/2";
        let div_bound = sqlite_family_mechanical_len_upper_bound(with_div).expect("div");
        assert_eq!(
            div_bound,
            with_div.len() + SQLITE_FAMILY_DIV_MOD_NULLIF_EXTRA
        );
        let insert = "INSERT OR IGNORE INTO t SELECT 1";
        let ins = sqlite_family_mechanical_len_upper_bound(insert).expect("insert");
        assert_eq!(ins, insert.len() + SQLITE_FAMILY_INSERT_OR_IGNORE_MAX_EXTRA);
        sqlite_family_mechanical_len_upper_bound("SELECT 1 /* ").expect_err("unterminated");
    }

    #[test]
    fn like_glob_wrap_affixes_match_documented_overhead() {
        assert_eq!(
            LIKE_GLOB_PATTERN_WRAP_BYTES,
            LIKE_GLOB_WRAP_PREFIX.len() + LIKE_GLOB_WRAP_SUFFIX.len()
        );
        assert_eq!(
            LIKE_GLOB_REWRITE_OVERHEAD,
            LIKE_GLOB_PATTERN_WRAP_BYTES + LIKE_TO_GLOB_KEYWORD_EXTRA
        );
        let wrapped = format!("{}x{}", LIKE_GLOB_WRAP_PREFIX, LIKE_GLOB_WRAP_SUFFIX);
        assert_eq!(wrapped.len(), 1 + LIKE_GLOB_PATTERN_WRAP_BYTES);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        })]

        #[test]
        fn pack_never_panics_on_arbitrary_text(s in r"[\x01-\x7f]{0,200}") {
            let _ = sql_v1_pack_statements(&s);
        }

        #[test]
        fn semicolon_inside_string_stays_one_statement(inner in r"[^\x00']{0,40}") {
            let sql = format!("SELECT '{inner};still'");
            let stmts = sql_v1_pack_statements(&sql).expect("quoted semicolon");
            prop_assert_eq!(stmts.len(), 1);
        }

        #[test]
        fn unterminated_delimiters_never_pack(
            prefix in r"[A-Za-z0-9 ]{0,40}",
            kind in 0u8..5,
            tail in r"[A-Za-z0-9 .,;]{0,80}"
        ) {
            let sql = match kind {
                0 => format!("SELECT {prefix} /* {tail}"),
                1 => format!("SELECT {prefix} '{tail}"),
                2 => format!("SELECT {prefix} \"{tail}"),
                3 => format!("SELECT {prefix} `{tail}"),
                _ => format!("SELECT {prefix} [{tail}"),
            };
            prop_assert!(
                sql_v1_pack_statements(&sql).is_err(),
                "malformed delimiters packed as success: {sql:?}"
            );
        }

        #[test]
        fn mechanical_preflight_never_exceeds_length_formula(
            likes in 0usize..20,
            divs in 0usize..8
        ) {
            let mut sql = String::from("SELECT json_extract(ifnull(body, '{}'), '$.k') FROM t WHERE 1=1");
            for _ in 0..likes {
                sql.push_str(" OR x LIKE '[%]_?*'");
            }
            for _ in 0..divs {
                sql.push_str(" AND 1/2");
            }
            let pre = sqlite_family_mechanical_len_upper_bound(&sql).expect("preflight");
            prop_assert!(
                pre <= sqlite_family_like_divmod_insert_upper_bound(sql.len()),
                "counted preflight {pre} exceeded length formula for {sql:?}"
            );
        }
    }
}
