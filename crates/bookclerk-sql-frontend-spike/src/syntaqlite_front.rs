//! Syntaqlite parse + Analyzer frontend (host-only).

use bookclerk_plugin_abi::sql_types::SqlTypeEnv;
use syntaqlite::analysis::{
    AritySpec, CatalogLayer, FunctionCategory, LineageResult, PhysicalTableAccess,
};
use syntaqlite::parse::{Parser, ParserConfig, TokenType};
use syntaqlite::{
    sqlite_dialect, AnalysisConfig, AnalysisContext, Analyzer, Catalog, ParseOutcome,
};

use crate::allowlist::{allowlist_tokens, TokenSpan};
use crate::{FrontendKind, FrontendShape, SpikeError};

const SQL_V1_FUNCS: &[(&str, FunctionCategory, AritySpec)] = &[
    ("ABS", FunctionCategory::Scalar, AritySpec::Exact(1)),
    ("LENGTH", FunctionCategory::Scalar, AritySpec::Exact(1)),
    ("LOWER", FunctionCategory::Scalar, AritySpec::Exact(1)),
    ("UPPER", FunctionCategory::Scalar, AritySpec::Exact(1)),
    ("TRIM", FunctionCategory::Scalar, AritySpec::Exact(1)),
    ("IFNULL", FunctionCategory::Scalar, AritySpec::Exact(2)),
    ("COALESCE", FunctionCategory::Scalar, AritySpec::AtLeast(1)),
    ("NULLIF", FunctionCategory::Scalar, AritySpec::Exact(2)),
    ("JSON", FunctionCategory::Scalar, AritySpec::Exact(1)),
    (
        "JSON_ARRAY",
        FunctionCategory::Scalar,
        AritySpec::AtLeast(0),
    ),
    (
        "JSON_OBJECT",
        FunctionCategory::Scalar,
        AritySpec::AtLeast(0),
    ),
    (
        "JSON_EXTRACT",
        FunctionCategory::Scalar,
        AritySpec::AtLeast(2),
    ),
    ("JSON_VALID", FunctionCategory::Scalar, AritySpec::Exact(1)),
    ("JSON_TYPE", FunctionCategory::Scalar, AritySpec::Exact(1)),
    ("MIN", FunctionCategory::Scalar, AritySpec::AtLeast(1)),
    ("MAX", FunctionCategory::Scalar, AritySpec::AtLeast(1)),
    ("COUNT", FunctionCategory::Aggregate, AritySpec::AtLeast(0)),
    ("SUM", FunctionCategory::Aggregate, AritySpec::Exact(1)),
    ("AVG", FunctionCategory::Aggregate, AritySpec::Exact(1)),
    ("REPLACE", FunctionCategory::Scalar, AritySpec::Exact(3)),
    ("ROUND", FunctionCategory::Scalar, AritySpec::AtLeast(1)),
    ("SUBSTR", FunctionCategory::Scalar, AritySpec::AtLeast(2)),
];

/// Parses `sql` with Syntaqlite, applies the SQL-v1 allowlist, then analyzes.
///
/// # Errors
///
/// Returns [`SpikeError`] when parse, allowlist, or strict-schema analysis fails.
pub fn parse_and_shape(sql: &str, env: &SqlTypeEnv) -> Result<FrontendShape, SpikeError> {
    let parser = Parser::with_config(&ParserConfig::default().with_collect_tokens(true));
    let mut session = parser.parse(sql);
    let stmt = match session.next() {
        ParseOutcome::Ok(stmt) => stmt,
        ParseOutcome::Err(err) => return Err(SpikeError::Parse(err.to_string())),
        ParseOutcome::Done => return Err(SpikeError::Parse("no statement".into())),
    };

    let token_spans: Vec<TokenSpan> = stmt
        .tokens()
        .map(|t| {
            let start = t.offset().as_usize();
            let end = start + t.length().as_usize();
            TokenSpan {
                start,
                end,
                text: t.text().to_string(),
                is_ident: matches!(t.token_type(), TokenType::Id) || ident_quoted(t.text()),
                is_trivia_or_literal: matches!(
                    t.token_type(),
                    TokenType::String | TokenType::Integer | TokenType::Float | TokenType::Blob
                ) || t.text().starts_with("--")
                    || t.text().starts_with("/*"),
            }
        })
        .collect();
    let token_count = token_spans.len();
    let placeholder_spans: Vec<(usize, usize)> = token_spans
        .iter()
        .filter(|t| t.text == "?")
        .map(|t| (t.start, t.end))
        .collect();

    if let Err(rej) = allowlist_tokens(sql, &token_spans) {
        return Err(SpikeError::Allowlist(rej.reason));
    }

    let mut catalog = Catalog::new(sqlite_dialect());
    {
        let layer = catalog.layer_mut(CatalogLayer::Database);
        let mut tables: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (table, column, _) in env.iter() {
            tables
                .entry(table.to_string())
                .or_default()
                .push(column.to_string());
        }
        for (name, cols) in tables {
            layer.insert_table(name, Some(cols), false);
        }
        for &(name, cat, arity) in SQL_V1_FUNCS {
            layer.insert_function_overload(name.to_string(), cat, arity);
        }
    }

    let mut analyzer = Analyzer::new();
    let mut ctx = AnalysisContext::new(&mut catalog)
        .with_config(AnalysisConfig::default().with_strict_schema());
    let model = analyzer.analyze(sql, &mut ctx);
    let diagnostics: Vec<String> = model
        .diagnostics()
        .map(|d| d.message().to_string())
        .collect();
    if !diagnostics.is_empty() {
        return Err(SpikeError::Analyze(diagnostics.join("; ")));
    }

    let physical_tables = model
        .statements()
        .iter()
        .flat_map(|s| match s.physical_tables_accessed() {
            Some(LineageResult::Complete(tables) | LineageResult::Partial(tables)) => tables
                .iter()
                .map(|t: &PhysicalTableAccess| t.name.clone())
                .collect::<Vec<_>>(),
            None => Vec::new(),
        })
        .collect();

    let output_lineage = match model.lineage() {
        Some(LineageResult::Complete(cols) | LineageResult::Partial(cols)) => cols
            .iter()
            .map(|c| {
                let origin = c
                    .origin
                    .as_ref()
                    .map(|o| format!("{}.{}", o.table, o.column));
                (c.name.clone(), origin)
            })
            .collect(),
        None => Vec::new(),
    };

    let kind = classify_kind(sql);
    let _ = stmt.root();

    Ok(FrontendShape {
        kind,
        physical_tables,
        output_lineage,
        placeholder_spans,
        token_count,
        diagnostics: Vec::new(),
        backend: "syntaqlite",
    })
}

fn ident_quoted(text: &str) -> bool {
    text.starts_with('"') || text.starts_with('`') || text.starts_with('[')
}

fn classify_kind(sql: &str) -> FrontendKind {
    let t = sql.trim_start();
    let head = t
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match head.as_str() {
        "SELECT" | "WITH" | "VALUES" => FrontendKind::Query,
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" => FrontendKind::Dml,
        "CREATE" | "DROP" | "ALTER" => FrontendKind::Ddl,
        _ => FrontendKind::Unknown,
    }
}
