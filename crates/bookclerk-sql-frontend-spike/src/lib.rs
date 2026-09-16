//! Host-private SQL-v1 frontend spike.
//!
//! Compares Syntaqlite and sqlparser-rs against the Bookclerk SQL-v1 corpus
//! without exposing parser-library types on the public ABI.

#![allow(
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

pub mod allowlist;
pub mod corpus;
mod sqlparser_front;
mod syntaqlite_front;

use bookclerk_plugin_abi::{validate_sql_v1_grammar, SqlType, SqlTypeEnv};
use corpus::{corpus, default_schema_sql, CorpusCase, GrammarExpect};

/// Bookclerk-owned shape produced by a frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendShape {
    /// Coarse statement class.
    pub kind: FrontendKind,
    /// Physical tables after CTE subtraction (folded).
    pub physical_tables: Vec<String>,
    /// SELECT-list lineage when the library provides it (`table.column`).
    pub output_lineage: Vec<(String, Option<String>)>,
    /// `?` placeholder spans in canonical SQL.
    pub placeholder_spans: Vec<(usize, usize)>,
    /// Token count (for lowering-site coverage).
    pub token_count: usize,
    /// Analyzer diagnostics (empty on success).
    pub diagnostics: Vec<String>,
    /// Frontend id (`syntaqlite` / `sqlparser`).
    pub backend: &'static str,
}

/// Coarse statement kind (not Cap’n `DbPlanStatementKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendKind {
    /// SELECT / VALUES / set-op.
    Query,
    /// INSERT / UPDATE / DELETE.
    Dml,
    /// CREATE / DROP / INDEX.
    Ddl,
    /// Unclassified.
    Unknown,
}

/// Spike failure (parse, allowlist, or analysis).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpikeError {
    /// Library parser rejected the text.
    Parse(String),
    /// Parsed but outside SQL-v1.
    Allowlist(String),
    /// Syntaqlite Analyzer diagnostic (strict schema).
    Analyze(String),
}

impl std::fmt::Display for SpikeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(s) => write!(f, "parse: {s}"),
            Self::Allowlist(s) => write!(f, "allowlist: {s}"),
            Self::Analyze(s) => write!(f, "analyze: {s}"),
        }
    }
}

/// One corpus row compared across Bookclerk grammar and both libraries.
#[derive(Debug, Clone)]
pub struct CorpusRow {
    /// Case id.
    pub id: String,
    /// Bookclerk `validate_sql_v1_grammar` result.
    pub bookclerk_ok: bool,
    /// Syntaqlite parse+allowlist+analyze.
    pub syntaqlite: Result<FrontendShape, SpikeError>,
    /// sqlparser-rs parse+allowlist+residual names.
    pub sqlparser: Result<FrontendShape, SpikeError>,
}

/// Default type environment matching [`default_schema_sql`].
#[must_use]
pub fn default_type_env() -> SqlTypeEnv {
    let mut env = SqlTypeEnv::new();
    for ddl in default_schema_sql() {
        bookclerk_plugin_abi::apply_schema_sql_to_env(&mut env, ddl);
    }
    let _ = SqlType::Integer;
    env
}

/// Run the extracted corpus through Bookclerk grammar and both frontends.
#[must_use]
pub fn run_corpus() -> Vec<CorpusRow> {
    let env = default_type_env();
    corpus()
        .into_iter()
        .map(|case: CorpusCase| {
            let bookclerk_ok = validate_sql_v1_grammar(case.sql, true).is_ok();
            CorpusRow {
                id: case.id.to_string(),
                bookclerk_ok,
                syntaqlite: syntaqlite_front::parse_and_shape(case.sql, &env),
                sqlparser: sqlparser_front::parse_and_shape(case.sql, &env),
            }
        })
        .collect()
}

/// True when a frontend result matches the grammar expectation.
#[must_use]
pub fn matches_expect(expect: GrammarExpect, result: &Result<FrontendShape, SpikeError>) -> bool {
    match expect {
        GrammarExpect::Admit => result.is_ok(),
        GrammarExpect::Reject => result.is_err(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::GrammarExpect;

    #[test]
    fn corpus_covers_required_constructs() {
        let tags: std::collections::BTreeSet<&str> = corpus()
            .into_iter()
            .flat_map(|c| c.tags.iter().copied())
            .collect();
        for need in [
            "cte",
            "recursive-cte",
            "explicit-cte-columns",
            "correlated-exists",
            "derived-tables",
            "aliases",
            "insert-values",
            "insert-select",
            "insert-with",
            "update",
            "delete",
            "returning",
            "union",
            "group-by",
            "case",
            "cast",
            "check",
            "defaults",
            "pk",
            "unique",
            "foreign-keys",
            "create-table",
            "create-index",
            "drop",
            "insert-or-ignore",
            "autoincrement",
            "placeholders",
            "json",
            "quoted-strings",
            "comments",
            "excluded",
        ] {
            assert!(tags.contains(need), "missing tag {need}");
        }
    }

    #[test]
    fn frontend_comparison_report() {
        let cases = corpus();
        let rows = run_corpus();
        let mut syn_agree = 0usize;
        let mut syn_false_admit = 0usize;
        let mut syn_false_reject = 0usize;
        let mut sql_agree = 0usize;
        let mut sql_false_admit = 0usize;
        let mut sql_false_reject = 0usize;
        let mut syn_tables = 0usize;
        let mut sql_tables = 0usize;
        let mut syn_lineage = 0usize;
        let mut syn_placeholders = 0usize;
        println!("id\texpect\tbookclerk\tsyntaqlite\tsqlparser");
        for (case, row) in cases.iter().zip(rows.iter()) {
            let syn_ok = row.syntaqlite.is_ok();
            let sql_ok = row.sqlparser.is_ok();
            println!(
                "{}\t{:?}\t{}\t{}\t{}",
                case.id,
                case.expect,
                row.bookclerk_ok,
                match &row.syntaqlite {
                    Ok(s) => format!("ok tables={:?}", s.physical_tables),
                    Err(e) => e.to_string(),
                },
                match &row.sqlparser {
                    Ok(s) => format!("ok tables={:?}", s.physical_tables),
                    Err(e) => e.to_string(),
                }
            );
            let expect_ok = case.expect == GrammarExpect::Admit;
            if syn_ok == expect_ok {
                syn_agree += 1;
            } else if syn_ok {
                syn_false_admit += 1;
            } else {
                syn_false_reject += 1;
            }
            if sql_ok == expect_ok {
                sql_agree += 1;
            } else if sql_ok {
                sql_false_admit += 1;
            } else {
                sql_false_reject += 1;
            }
            if let Ok(s) = &row.syntaqlite {
                syn_tables += s.physical_tables.len();
                syn_lineage += s.output_lineage.len();
                syn_placeholders += s.placeholder_spans.len();
            }
            if let Ok(s) = &row.sqlparser {
                sql_tables += s.physical_tables.len();
            }
            let _ = syn_ok;
            let _ = sql_ok;
        }
        println!(
            "\nSUMMARY n={} syn_agree={} syn_false_admit={} syn_false_reject={} sql_agree={} sql_false_admit={} sql_false_reject={} syn_tables={} sql_tables={} syn_lineage={} syn_placeholders={}",
            rows.len(),
            syn_agree,
            syn_false_admit,
            syn_false_reject,
            sql_agree,
            sql_false_admit,
            sql_false_reject,
            syn_tables,
            sql_tables,
            syn_lineage,
            syn_placeholders
        );
        assert!(!rows.is_empty());
    }
}
