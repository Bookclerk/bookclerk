//! SQL-v1 spike corpus: positive and negative statements from PR #179 tests.

/// Whether Bookclerk SQL v1 admits the statement at the grammar layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrammarExpect {
    /// Fail-closed grammar admits this form.
    Admit,
    /// Grammar (or binding DDL grammar) rejects this form.
    Reject,
}

/// One corpus statement.
#[derive(Debug, Clone)]
pub struct CorpusCase {
    /// Stable id.
    pub id: &'static str,
    /// Canonical SQL.
    pub sql: &'static str,
    /// Grammar-layer expectation.
    pub expect: GrammarExpect,
    /// Construct tags (cte, insert-select, excluded, …).
    pub tags: &'static [&'static str],
}

/// Default catalog used when analyzing DML against physical tables.
#[must_use]
pub fn default_schema_sql() -> &'static [&'static str] {
    &[
        "CREATE TABLE books (id INTEGER, title TEXT, body TEXT, token TEXT)",
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, flag BOOLEAN)",
        "CREATE TABLE typed (v TEXT, n INTEGER, r REAL, payload TEXT, blob BLOB)",
        "CREATE TABLE ign_sel (id INTEGER)",
        "CREATE TABLE identities (id INTEGER, user_id INTEGER)",
        "CREATE TABLE users (id INTEGER, role TEXT, status TEXT)",
        "CREATE TABLE jobs (payload TEXT)",
        "CREATE TABLE t (id INTEGER, a INTEGER, b INTEGER, n INTEGER, x INTEGER, body TEXT)",
        "CREATE TABLE u (id INTEGER, x INTEGER)",
        "CREATE TABLE peer (id INTEGER)",
        "CREATE TABLE anything_i_own (id INTEGER)",
    ]
}

/// Extracted SQL-v1 matrix (existing tests + plan constructs).
#[must_use]
pub fn corpus() -> Vec<CorpusCase> {
    let mut cases = Vec::new();
    let mut push = |id, sql, expect, tags| {
        cases.push(CorpusCase {
            id,
            sql,
            expect,
            tags,
        });
    };

    // Positive DML / queries
    push(
        "select-bind",
        "SELECT id FROM books WHERE id = ?",
        GrammarExpect::Admit,
        &["select", "placeholders"],
    );
    push(
        "select-like-nulls",
        "SELECT body FROM books WHERE body LIKE 'a%' ORDER BY id ASC NULLS FIRST",
        GrammarExpect::Admit,
        &["like", "order-by", "nulls"],
    );
    push(
        "select-placeholder-in-string",
        "SELECT id FROM books WHERE id = '?' OR id = '$1' -- ?1\nAND body = ?",
        GrammarExpect::Admit,
        &["placeholders", "comments", "quoted-strings"],
    );
    push(
        "insert-values",
        "INSERT INTO books (id) VALUES (?)",
        GrammarExpect::Admit,
        &["insert-values", "placeholders"],
    );
    push(
        "insert-or-ignore-with",
        "INSERT OR IGNORE INTO ign_sel (id) WITH s(id) AS (SELECT 1) SELECT * FROM s RETURNING id",
        GrammarExpect::Admit,
        &[
            "insert-or-ignore",
            "insert-select",
            "insert-with",
            "returning",
            "cte",
        ],
    );
    push(
        "cte-select",
        "WITH seed AS (SELECT 1 AS id) SELECT * FROM seed",
        GrammarExpect::Admit,
        &["cte", "select-alias"],
    );
    push(
        "cte-explicit-cols",
        "WITH c(x) AS (SELECT id FROM books) SELECT x FROM c",
        GrammarExpect::Admit,
        &["cte", "explicit-cte-columns"],
    );
    push(
        "recursive-cte",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
        GrammarExpect::Admit,
        &["recursive-cte", "union"],
    );
    push(
        "recursive-cte-self-scope",
        "WITH RECURSIVE t(x) AS (SELECT id FROM books UNION ALL SELECT t.x FROM t WHERE t.x < 3) SELECT t.x FROM t",
        GrammarExpect::Admit,
        &["recursive-cte", "aliases"],
    );
    push(
        "derived-table",
        "SELECT o.status FROM (SELECT 'ok' AS status) o",
        GrammarExpect::Admit,
        &["derived-tables", "select-alias"],
    );
    push(
        "table-alias",
        "SELECT b.id FROM books AS b",
        GrammarExpect::Admit,
        &["aliases"],
    );
    push(
        "table-alias-bare",
        "SELECT b.id FROM books b",
        GrammarExpect::Admit,
        &["aliases"],
    );
    push(
        "join",
        "SELECT i.user_id FROM books t JOIN identities i ON i.id = t.id",
        GrammarExpect::Admit,
        &["join", "aliases"],
    );
    push(
        "correlated-exists",
        "UPDATE identities SET user_id = 1 WHERE NOT EXISTS (SELECT 1 FROM identities i WHERE i.id = identities.id AND i.user_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM users WHERE id = i.user_id))",
        GrammarExpect::Admit,
        &["update", "correlated-exists"],
    );
    push(
        "in-subquery",
        "DELETE FROM books WHERE id IN (SELECT i.user_id FROM identities i JOIN users u ON u.id = i.user_id WHERE i.user_id IS NOT NULL)",
        GrammarExpect::Admit,
        &["delete", "correlated-in", "join"],
    );
    push(
        "insert-select",
        "INSERT INTO ign_sel (id) SELECT id FROM books",
        GrammarExpect::Admit,
        &["insert-select"],
    );
    push(
        "update-set",
        "UPDATE t SET n = 1 WHERE id = ?",
        GrammarExpect::Admit,
        &["update", "placeholders"],
    );
    push(
        "delete",
        "DELETE FROM anything_i_own",
        GrammarExpect::Admit,
        &["delete"],
    );
    push(
        "union-all",
        "SELECT 1 AS n UNION ALL SELECT 2 AS n",
        GrammarExpect::Admit,
        &["union"],
    );
    push(
        "group-by-having",
        "SELECT id, count(*) FROM books GROUP BY id HAVING count(*) > 0",
        GrammarExpect::Admit,
        &["group-by", "having", "aggregates"],
    );
    push(
        "case-cast",
        "SELECT CASE WHEN id IS NULL THEN 0 ELSE CAST(id AS INTEGER) END FROM books",
        GrammarExpect::Admit,
        &["case", "cast", "null"],
    );
    push(
        "json-helpers",
        "SELECT ifnull(body, ''), json_extract(body, '$.k'), json_valid(body), json_object('k', body) FROM notes",
        GrammarExpect::Admit,
        &["json", "ifnull"],
    );
    push(
        "scalar-min-max",
        "SELECT min(1, 2), max(a, b, c) FROM t",
        GrammarExpect::Admit,
        &["min-max"],
    );
    push(
        "insert-returning",
        "INSERT INTO t (id) VALUES (?) RETURNING id",
        GrammarExpect::Admit,
        &["insert-values", "returning", "placeholders"],
    );
    push(
        "cte-insert",
        "WITH seed AS (SELECT 1 AS id) INSERT INTO books (id) SELECT * FROM seed",
        GrammarExpect::Admit,
        &["cte", "insert-select"],
    );
    push(
        "parenthesized-expr",
        "SELECT (id + 1) FROM books",
        GrammarExpect::Admit,
        &["arithmetic"],
    );
    push(
        "select-as-alias",
        "SELECT id AS label FROM books",
        GrammarExpect::Admit,
        &["select-alias"],
    );
    push(
        "create-table",
        "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT, flag BOOLEAN)",
        GrammarExpect::Admit,
        &["create-table", "pk", "boolean"],
    );
    push(
        "create-table-autoincrement",
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER)",
        GrammarExpect::Admit,
        &["create-table", "autoincrement", "pk"],
    );
    push(
        "create-table-check",
        "CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (n > 0))",
        GrammarExpect::Admit,
        &["create-table", "check"],
    );
    push(
        "create-table-fk",
        "CREATE TABLE IF NOT EXISTS keyed (id INTEGER PRIMARY KEY, other_id INTEGER REFERENCES peer(id))",
        GrammarExpect::Admit,
        &["create-table", "foreign-keys", "pk"],
    );
    push(
        "create-table-unique-fk-table",
        "CREATE TABLE IF NOT EXISTS keyed2 (id INTEGER, other_id INTEGER, UNIQUE (id), FOREIGN KEY (other_id) REFERENCES peer(id))",
        GrammarExpect::Admit,
        &["create-table", "unique", "foreign-keys"],
    );
    push(
        "create-table-defaults",
        "CREATE TABLE IF NOT EXISTS typed_defaults (v TEXT DEFAULT 'x', n REAL, payload BLOB DEFAULT X'deadbeef', n2 INTEGER DEFAULT CAST(1 AS INTEGER))",
        GrammarExpect::Admit,
        &["create-table", "defaults", "cast"],
    );
    push(
        "create-index",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_body ON notes(body)",
        GrammarExpect::Admit,
        &["create-index"],
    );
    push(
        "drop-table",
        "DROP TABLE IF EXISTS memos",
        GrammarExpect::Admit,
        &["drop"],
    );
    push(
        "create-check-length",
        "CREATE TABLE IF NOT EXISTS t (v TEXT CHECK (length(v) > 0))",
        GrammarExpect::Admit,
        &["create-table", "check", "functions"],
    );

    // Deliberately excluded
    push(
        "reject-dollar-placeholder",
        "SELECT id FROM books WHERE id = $1",
        GrammarExpect::Reject,
        &["placeholders", "excluded"],
    );
    push(
        "reject-nnn-placeholder",
        "SELECT id FROM books WHERE id = ?1",
        GrammarExpect::Reject,
        &["placeholders", "excluded"],
    );
    push(
        "reject-ilike",
        "SELECT body FROM books WHERE body ILIKE 'a%'",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-distinct-on",
        "SELECT DISTINCT ON (id) id FROM books",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-double-colon",
        "SELECT id::text FROM books",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-cast-bytea",
        "SELECT CAST(id AS BYTEA) FROM books",
        GrammarExpect::Reject,
        &["excluded", "cast"],
    );
    push(
        "reject-quoted-fn",
        r#"SELECT "hex"(id) FROM books"#,
        GrammarExpect::Reject,
        &["excluded", "quoted-idents"],
    );
    push(
        "reject-quoted-table",
        r#"CREATE TABLE IF NOT EXISTS "Foo" (id INTEGER PRIMARY KEY)"#,
        GrammarExpect::Reject,
        &["excluded", "quoted-idents", "create-table"],
    );
    push(
        "reject-autoincrement-without-pk",
        "CREATE TABLE IF NOT EXISTS t (id INTEGER AUTOINCREMENT)",
        GrammarExpect::Reject,
        &["excluded", "autoincrement"],
    );
    push(
        "reject-real-autoincrement",
        "CREATE TABLE IF NOT EXISTS t (id REAL PRIMARY KEY AUTOINCREMENT)",
        GrammarExpect::Reject,
        &["excluded", "autoincrement"],
    );
    push(
        "reject-glob",
        "SELECT body FROM notes WHERE body GLOB 'A*'",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-collate",
        "SELECT body FROM notes WHERE body COLLATE NOCASE = 'x'",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-replace-into",
        "REPLACE INTO notes (id) VALUES (1)",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-insert-or-replace",
        "INSERT OR REPLACE INTO notes (id) VALUES (1)",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-insert-or-abort",
        "INSERT OR ABORT INTO notes (id) VALUES (1)",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-strict",
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY) STRICT",
        GrammarExpect::Reject,
        &["excluded", "create-table"],
    );
    push(
        "reject-without-rowid",
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY) WITHOUT ROWID",
        GrammarExpect::Reject,
        &["excluded", "create-table"],
    );
    push(
        "reject-jsonb",
        "CREATE TABLE IF NOT EXISTS t (doc JSONB)",
        GrammarExpect::Reject,
        &["excluded", "create-table"],
    );
    push(
        "reject-numeric",
        "CREATE TABLE IF NOT EXISTS t (n NUMERIC(10, 2))",
        GrammarExpect::Reject,
        &["excluded", "create-table"],
    );
    push(
        "reject-varchar",
        "CREATE TABLE IF NOT EXISTS t (v VARCHAR(255))",
        GrammarExpect::Reject,
        &["excluded", "create-table"],
    );
    push(
        "reject-bool-alias",
        "CREATE TABLE IF NOT EXISTS t (flag BOOL)",
        GrammarExpect::Reject,
        &["excluded", "create-table"],
    );
    push(
        "reject-drop-cascade",
        "DROP TABLE IF EXISTS notes CASCADE",
        GrammarExpect::Reject,
        &["excluded", "drop"],
    );
    push(
        "reject-index-using",
        "CREATE INDEX IF NOT EXISTS i ON notes USING btree (body)",
        GrammarExpect::Reject,
        &["excluded", "create-index"],
    );
    push(
        "reject-index-include",
        "CREATE INDEX IF NOT EXISTS i ON notes (body) INCLUDE (id)",
        GrammarExpect::Reject,
        &["excluded", "create-index"],
    );
    push(
        "reject-window",
        "SELECT id, row_number() OVER (ORDER BY id) FROM books",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-right-join",
        "SELECT * FROM books RIGHT JOIN notes ON books.id = notes.id",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-full-join",
        "SELECT * FROM books FULL JOIN notes ON books.id = notes.id",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-except",
        "SELECT id FROM books EXCEPT SELECT id FROM notes",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-pragma",
        "PRAGMA user_version",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-create-view",
        "CREATE VIEW v AS SELECT 1",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-create-trigger",
        "CREATE TRIGGER trg AFTER INSERT ON notes BEGIN SELECT 1; END",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-qualified",
        "SELECT * FROM public.books",
        GrammarExpect::Reject,
        &["excluded"],
    );
    push(
        "reject-round-arity",
        "SELECT round(id, 2, 3) FROM books",
        GrammarExpect::Reject,
        &["excluded", "functions"],
    );
    push(
        "reject-default-paren",
        "CREATE TABLE IF NOT EXISTS t (n INTEGER DEFAULT (1))",
        GrammarExpect::Reject,
        &["excluded", "defaults"],
    );
    push(
        "reject-ident-64",
        "CREATE TABLE IF NOT EXISTS aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (id INTEGER PRIMARY KEY)",
        GrammarExpect::Reject,
        &["excluded", "identifiers"],
    );
    push(
        "reject-ambiguous-would-need-types",
        "SELECT missing FROM typed",
        GrammarExpect::Admit,
        &["ambiguous-columns"],
    );

    cases
}
