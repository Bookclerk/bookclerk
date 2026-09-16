//! sqlparser-rs SQLite dialect frontend (host-only). Residual name resolution
//! is a CTE-name subtractor — not a TypeCx port.

use bookclerk_plugin_abi::sql_types::SqlTypeEnv;
use sqlparser::ast::{SetExpr, Statement, TableFactor, Visit, Visitor};
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};
use std::ops::ControlFlow;

use crate::allowlist::{allowlist_tokens, TokenSpan};
use crate::{FrontendKind, FrontendShape, SpikeError};

struct ShapeVisitor {
    tables: Vec<String>,
    cte_names: Vec<String>,
}

impl Visitor for ShapeVisitor {
    type Break = ();

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, .. } = table_factor {
            if let Some(ident) = name.0.first().and_then(|p| p.as_ident()) {
                self.tables.push(ident.value.clone());
            }
        }
        ControlFlow::Continue(())
    }
}

fn cte_names(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::Query(q) => q
            .with
            .as_ref()
            .map(|w| {
                w.cte_tables
                    .iter()
                    .map(|c| c.alias.name.value.clone())
                    .collect()
            })
            .unwrap_or_default(),
        Statement::Insert(insert) => insert
            .source
            .as_ref()
            .and_then(|q| q.with.as_ref())
            .map(|w| {
                w.cte_tables
                    .iter()
                    .map(|c| c.alias.name.value.clone())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn statement_kind(stmt: &Statement) -> FrontendKind {
    match stmt {
        Statement::Query(q) => match q.body.as_ref() {
            SetExpr::Select(_) | SetExpr::SetOperation { .. } | SetExpr::Values(_) => {
                FrontendKind::Query
            }
            _ => FrontendKind::Query,
        },
        Statement::Insert(_) | Statement::Update { .. } | Statement::Delete(_) => FrontendKind::Dml,
        Statement::CreateTable(_) | Statement::Drop { .. } | Statement::CreateIndex(_) => {
            FrontendKind::Ddl
        }
        _ => FrontendKind::Unknown,
    }
}

/// Parses `sql` with sqlparser-rs, applies the SQL-v1 allowlist, then
/// subtracts CTE names from FROM tables.
///
/// # Errors
///
/// Returns [`SpikeError`] when parse or allowlist fails.
pub fn parse_and_shape(sql: &str, _env: &SqlTypeEnv) -> Result<FrontendShape, SpikeError> {
    let dialect = SQLiteDialect {};
    let stmts = Parser::parse_sql(&dialect, sql).map_err(|e| SpikeError::Parse(e.to_string()))?;
    let stmt = stmts
        .first()
        .ok_or_else(|| SpikeError::Parse("no statement".into()))?;

    let mut tokenizer = Tokenizer::new(&dialect, sql);
    let tokens = tokenizer
        .tokenize()
        .map_err(|e| SpikeError::Parse(e.to_string()))?;
    let token_spans = tokens_to_spans(sql, &tokens);
    let token_count = token_spans.len();
    let placeholder_spans: Vec<(usize, usize)> = token_spans
        .iter()
        .filter(|t| t.text == "?")
        .map(|t| (t.start, t.end))
        .collect();

    if let Err(rej) = allowlist_tokens(sql, &token_spans) {
        return Err(SpikeError::Allowlist(rej.reason));
    }

    let mut visitor = ShapeVisitor {
        tables: Vec::new(),
        cte_names: cte_names(stmt),
    };
    let _ = stmt.visit(&mut visitor);
    let physical_tables: Vec<String> = visitor
        .tables
        .into_iter()
        .filter(|t| !visitor.cte_names.iter().any(|c| c.eq_ignore_ascii_case(t)))
        .collect();

    Ok(FrontendShape {
        kind: statement_kind(stmt),
        physical_tables,
        output_lineage: Vec::new(),
        placeholder_spans,
        token_count,
        diagnostics: Vec::new(),
        backend: "sqlparser",
    })
}

fn tokens_to_spans(sql: &str, tokens: &[Token]) -> Vec<TokenSpan> {
    let mut cursor = 0usize;
    let mut out = Vec::with_capacity(tokens.len());
    for tok in tokens {
        if matches!(tok, Token::EOF) {
            continue;
        }
        let rendered = tok.to_string();
        let search = match tok {
            Token::Word(w) if w.quote_style.is_some() => w.to_string(),
            Token::Placeholder(p) => p.clone(),
            Token::Whitespace(Whitespace::SingleLineComment { prefix, comment }) => {
                format!("{prefix}{comment}")
            }
            Token::Whitespace(Whitespace::MultiLineComment(c)) => format!("/*{c}*/"),
            _ => rendered.clone(),
        };
        let start = sql[cursor..].find(&search).map_or(cursor, |i| cursor + i);
        let end = (start + search.len()).min(sql.len());
        cursor = end;
        let is_trivia = matches!(
            tok,
            Token::Whitespace(_)
                | Token::SingleQuotedString(_)
                | Token::NationalStringLiteral(_)
                | Token::HexStringLiteral(_)
                | Token::Number(_, _)
        );
        let is_ident = matches!(tok, Token::Word(_));
        let text = match tok {
            Token::Word(w) => w.to_string(),
            Token::Placeholder(p) => p.clone(),
            Token::Question => "?".into(),
            _ => rendered,
        };
        out.push(TokenSpan {
            start,
            end,
            text,
            is_ident,
            is_trivia_or_literal: is_trivia,
        });
    }
    out
}
