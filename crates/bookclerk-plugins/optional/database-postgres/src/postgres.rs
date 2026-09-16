//! PostgreSQL engine for the database plugin (SeaORM sqlx-postgres).

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, RuntimeErr, Statement};
use std::ops::Deref;

/// Minimum PostgreSQL major BookclerkSQL adapters accept.
pub const MIN_POSTGRES_MAJOR: u32 = 16;

/// `server_version_num` floor (`160000` for PostgreSQL 16).
pub const MIN_POSTGRES_VERSION_NUM: u32 = MIN_POSTGRES_MAJOR * 10_000;

/// Open Postgres with a host-mediated connection URL (ping only; host applies schema).
///
/// # Errors
///
/// Returns an error when the operation fails, the server is older than
/// [`MIN_POSTGRES_MAJOR`], or the session encoding is not UTF8.
pub async fn open(url: &str) -> std::result::Result<DatabaseConnection, DbErr> {
    let db = connect_engine(url).await?;
    db.ping().await?;
    require_postgres_readiness(&db).await?;
    tracing::debug!(plugin = "postgres", "opened library database");
    Ok(db)
}

/// SeaORM connect, rewritten through the workerd socket proxy when nested.
async fn connect_engine(url: &str) -> std::result::Result<DatabaseConnection, DbErr> {
    let url = crate::socket_mediate::mediated_connect_url(url).await?;
    Database::connect(url).await
}

/// TCP host/port a Postgres URL would dial (`None` for Unix-socket URLs).
///
/// Query `host` / `hostaddr` / `port` override the URL authority. A `host`
/// that starts with `/` is a libpq directory for `.s.PGSQL.{port}`.
#[must_use]
pub fn postgres_tcp_target(url: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(url).ok()?;
    let mut host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    let mut port = parsed.port().unwrap_or(5432);
    let mut unix_socket = parsed
        .host_str()
        .is_some_and(|h| h.starts_with('/') || h.starts_with("%2F") || h.starts_with("%2f"));
    for (key, value) in parsed.query_pairs() {
        match &*key {
            "host" if value.starts_with('/') => unix_socket = true,
            "host" => host = Some(value.into_owned()),
            "hostaddr" => host = Some(value.into_owned()),
            "port" => port = value.parse().ok()?,
            _ => {}
        }
    }
    if unix_socket {
        return None;
    }
    Some((host?, port))
}

/// Reject servers older than [`MIN_POSTGRES_MAJOR`] or without UTF8 encoding.
async fn require_postgres_readiness(db: &DatabaseConnection) -> std::result::Result<(), DbErr> {
    let ver = scalar_text(db, "SHOW server_version_num").await?;
    let num: u32 = ver.trim().parse().map_err(|_| {
        DbErr::Custom(format!(
            "postgres server_version_num is not an integer: {ver}"
        ))
    })?;
    if num < MIN_POSTGRES_VERSION_NUM {
        return Err(DbErr::Custom(format!(
            "BookclerkSQL requires PostgreSQL {MIN_POSTGRES_MAJOR}+ (server_version_num {num})"
        )));
    }
    let server = scalar_text(db, "SHOW server_encoding").await?;
    if !is_utf8_encoding(&server) {
        return Err(DbErr::Custom(format!(
            "BookclerkSQL requires UTF8 server_encoding (got {server})"
        )));
    }
    let client = scalar_text(db, "SHOW client_encoding").await?;
    if !is_utf8_encoding(&client) {
        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            "SET client_encoding TO 'UTF8'".to_string(),
        ))
        .await?;
        let client = scalar_text(db, "SHOW client_encoding").await?;
        if !is_utf8_encoding(&client) {
            return Err(DbErr::Custom(format!(
                "BookclerkSQL requires UTF8 client_encoding (got {client})"
            )));
        }
    }
    Ok(())
}

/// True when Postgres reports UTF8 / UTF-8 / UNICODE for an encoding GUC.
fn is_utf8_encoding(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_uppercase().as_str(),
        "UTF8" | "UTF-8" | "UNICODE"
    )
}

/// First column of a one-row `SHOW` / scalar query as text.
async fn scalar_text(db: &DatabaseConnection, sql: &str) -> std::result::Result<String, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_string(
            db.get_database_backend(),
            sql.to_string(),
        ))
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| DbErr::Custom(format!("{sql} returned no rows")))?;
    row.try_get_by_index::<String>(0)
        .or_else(|_| row.try_get_by_index::<i32>(0).map(|n| n.to_string()))
        .or_else(|_| row.try_get_by_index::<i64>(0).map(|n| n.to_string()))
}

/// Rewrites the database name in a Postgres URL, preserving query options.
#[must_use]
pub fn postgres_url_with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let trimmed = base.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(slash) => {
            let head = &trimmed[..slash];
            match query {
                Some(q) => format!("{head}/{database}?{q}"),
                None => format!("{head}/{database}"),
            }
        }
        None => match query {
            Some(q) => format!("{trimmed}/{database}?{q}"),
            None => format!("{trimmed}/{database}"),
        },
    }
}

/// True when `CREATE DATABASE` lost a check-then-create race.
///
/// Postgres may report `42P04 duplicate_database` or a catalog unique
/// violation (`23505` / `pg_database_datname_index`) depending on version
/// and timing. Callers still re-check `pg_database` after a miss.
fn is_duplicate_database(err: &DbErr) -> bool {
    match err {
        DbErr::Exec(RuntimeErr::SqlxError(e))
        | DbErr::Query(RuntimeErr::SqlxError(e))
        | DbErr::Conn(RuntimeErr::SqlxError(e)) => match e.deref() {
            sea_orm::sqlx::Error::Database(db_err) => {
                matches!(db_err.code().as_deref(), Some("42P04" | "23505"))
            }
            _ => duplicate_database_text(&err.to_string()),
        },
        _ => duplicate_database_text(&err.to_string()),
    }
}

/// True when `text` names a concurrent `CREATE DATABASE` race.
fn duplicate_database_text(text: &str) -> bool {
    text.contains("42P04")
        || text.contains("duplicate_database")
        || text.contains("pg_database_datname_index")
}

/// True when `database` already exists in this cluster.
async fn binding_database_exists(
    admin: &DatabaseConnection,
    database: &str,
) -> std::result::Result<bool, DbErr> {
    let rows = admin
        .query_all_raw(Statement::from_string(
            admin.get_database_backend(),
            format!("SELECT 1 FROM pg_database WHERE datname = '{database}'"),
        ))
        .await?;
    Ok(!rows.is_empty())
}

/// True when `name` is a safe unquoted Postgres identifier (`[a-z0-9_]+`).
fn binding_database_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Open a dedicated per-binding **database** (not a schema on the library DB).
///
/// Creates the database when missing, then connects to it. Plugin SQL cannot
/// see host library tables in another database, even via `CREATE TABLE AS`
/// or `REFERENCES public.books`.
///
/// # Errors
///
/// Returns an error when the name is unsafe, `CREATE DATABASE` fails (the
/// role needs `CREATEDB`), the binding connection cannot ping, or the
/// binding session is older than [`MIN_POSTGRES_MAJOR`] or not UTF8.
pub async fn open_binding(
    url: &str,
    database: &str,
) -> std::result::Result<DatabaseConnection, DbErr> {
    if !binding_database_name_ok(database) {
        return Err(DbErr::Custom(format!(
            "invalid binding database name `{database}`"
        )));
    }
    let admin = connect_engine(url).await?;
    if !binding_database_exists(&admin, database).await? {
        if let Err(err) = admin
            .execute_raw(Statement::from_string(
                admin.get_database_backend(),
                format!("CREATE DATABASE {database}"),
            ))
            .await
        {
            // 42P04 is the documented race; some clusters surface 23505 on
            // `pg_database_datname_index` instead. If the name exists now,
            // the other opener won — connect.
            if !is_duplicate_database(&err) && !binding_database_exists(&admin, database).await? {
                return Err(DbErr::Custom(format!(
                    "could not create isolated plugin database `{database}` \
                     (the Postgres role needs CREATEDB): {err}"
                )));
            }
        }
    }
    drop(admin);
    let db = connect_engine(&postgres_url_with_database(url, database)).await?;
    db.ping().await?;
    require_postgres_readiness(&db).await?;
    tracing::debug!(plugin = "postgres", database, "opened binding database");
    Ok(db)
}

/// Connects to an existing isolated binding database without creating it.
///
/// # Errors
///
/// Returns when the name is unsafe, the database is missing, ping fails, or
/// the binding session is older than [`MIN_POSTGRES_MAJOR`] or not UTF8.
pub async fn open_binding_existing(
    url: &str,
    database: &str,
) -> std::result::Result<DatabaseConnection, DbErr> {
    if !binding_database_name_ok(database) {
        return Err(DbErr::Custom(format!(
            "invalid binding database name `{database}`"
        )));
    }
    let admin = connect_engine(url).await?;
    if !binding_database_exists(&admin, database).await? {
        return Err(DbErr::Custom(format!(
            "plugin database `{database}` does not exist (lookup-only; will not provision)"
        )));
    }
    drop(admin);
    let db = connect_engine(&postgres_url_with_database(url, database)).await?;
    db.ping().await?;
    require_postgres_readiness(&db).await?;
    Ok(db)
}

/// Drops an isolated per-binding PostgreSQL database.
///
/// Connects to the admin URL (not `database`) so `DROP DATABASE` is legal.
/// Other backends attached to the target are terminated first. Missing
/// databases are success (`IF EXISTS`) so the registry row can be removed.
///
/// # Errors
///
/// Returns when the name is unsafe or `DROP DATABASE` fails.
pub async fn drop_binding(url: &str, database: &str) -> std::result::Result<(), DbErr> {
    if !binding_database_name_ok(database) {
        return Err(DbErr::Custom(format!(
            "invalid binding database name `{database}`"
        )));
    }
    let admin = connect_engine(url).await?;
    let backend = admin.get_database_backend();
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = '{database}' AND pid <> pg_backend_pid()"
    );
    let drop = format!("DROP DATABASE IF EXISTS {database}");
    let mut last_err = None;
    for attempt in 0..5 {
        let _ = admin
            .execute_raw(Statement::from_string(backend, terminate.clone()))
            .await;
        match admin
            .execute_raw(Statement::from_string(backend, drop.clone()))
            .await
        {
            Ok(_) => {
                tracing::debug!(plugin = "postgres", database, "dropped binding database");
                return Ok(());
            }
            Err(err) if is_database_in_use(&err) && attempt + 1 < 5 => {
                last_err = Some(err);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or_else(|| DbErr::Custom(format!("could not drop `{database}`"))))
}

/// True when `DROP DATABASE` failed because another session is still connected.
fn is_database_in_use(err: &DbErr) -> bool {
    let t = err.to_string().to_ascii_lowercase();
    t.contains("being accessed") || t.contains("55006")
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    fn postgres_test_url() -> String {
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap_or_else(|_| {
            panic!(
                "BOOKCLERK_TEST_POSTGRES_URL is required to run postgres binding tests \
                 (CI sets BOOKCLERK_REQUIRE_POSTGRES_TESTS=1)"
            )
        });
        assert!(
            !url.trim().is_empty(),
            "BOOKCLERK_TEST_POSTGRES_URL must not be empty"
        );
        url
    }

    #[test]
    fn open_binding_rejects_unsafe_database_names() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        for bad in ["", "Public", "a.b", "a\"b", "a b", "a;b"] {
            let err = rt
                .block_on(open_binding("postgres://invalid", bad))
                .expect_err("unsafe database name must fail before connecting");
            assert!(err.to_string().contains("database name"), "{bad}: {err}");
        }
    }

    #[test]
    fn drop_binding_rejects_unsafe_database_names() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        for bad in ["", "Public", "a.b", "a\"b", "a b", "a;b"] {
            let err = rt
                .block_on(drop_binding("postgres://invalid", bad))
                .expect_err("unsafe database name must fail before connecting");
            assert!(err.to_string().contains("database name"), "{bad}: {err}");
        }
    }

    #[test]
    fn postgres_url_with_database_preserves_query() {
        assert_eq!(
            postgres_url_with_database("postgres://h/library?sslmode=require", "pb_echo_db"),
            "postgres://h/pb_echo_db?sslmode=require"
        );
    }

    #[test]
    fn postgres_tcp_target_from_ci_url() {
        assert_eq!(
            postgres_tcp_target("postgres://postgres:postgres@localhost:5432/postgres"),
            Some(("localhost".into(), 5432))
        );
        assert_eq!(
            postgres_tcp_target("postgres://user:pass@127.0.0.1/bookclerk"),
            Some(("127.0.0.1".into(), 5432))
        );
        assert_eq!(
            postgres_tcp_target("postgres://user@db.example.com:6543/db"),
            Some(("db.example.com".into(), 6543))
        );
        assert_eq!(
            postgres_tcp_target("postgres:///?host=/var/run/postgresql&port=5432"),
            None
        );
        assert_eq!(
            postgres_tcp_target("postgres://u@h/db?host=10.0.0.5&port=5433"),
            Some(("10.0.0.5".into(), 5433))
        );
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_binding_databases_are_physically_separate() {
        let url = postgres_test_url();
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let db_a = format!("pb_test_a_{suffix}");
        let db_b = format!("pb_test_b_{suffix}");
        // Truncate to Postgres NAMEDATALEN (63) if the nanos suffix is long.
        let db_a = db_a.chars().take(63).collect::<String>();
        let db_b = db_b.chars().take(63).collect::<String>();
        let a = open_binding(&url, &db_a).await.expect("binding A");
        let b = open_binding(&url, &db_b).await.expect("binding B");
        for (db, marker) in [(&a, "alpha"), (&b, "beta")] {
            db.execute_raw(Statement::from_string(
                db.get_database_backend(),
                "CREATE TABLE notes (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)".to_string(),
            ))
            .await
            .expect("create per-binding table");
            db.execute_raw(Statement::from_string(
                db.get_database_backend(),
                format!("INSERT INTO notes (body) VALUES ('{marker}')"),
            ))
            .await
            .expect("insert per-binding row");
        }
        for (db, marker) in [(&a, "alpha"), (&b, "beta")] {
            let rows = db
                .query_all_raw(Statement::from_string(
                    db.get_database_backend(),
                    "SELECT body FROM notes ORDER BY id".to_string(),
                ))
                .await
                .expect("select per-binding rows");
            assert_eq!(rows.len(), 1, "each binding database is isolated");
            let body: String = rows[0].try_get("", "body").expect("body");
            assert_eq!(body, marker);
            let host = db
                .query_all_raw(Statement::from_string(
                    db.get_database_backend(),
                    "SELECT to_regclass('public.books') IS NOT NULL AS present".to_string(),
                ))
                .await
                .expect("host table probe");
            let present: bool = host[0].try_get("", "present").unwrap_or(true);
            assert!(
                !present,
                "binding database must not contain the host library catalog"
            );
        }
        drop(a);
        drop(b);
        let admin = Database::connect(url.as_str()).await.expect("admin");
        let backend = admin.get_database_backend();
        for name in [&db_a, &db_b] {
            admin
                .execute_raw(Statement::from_string(
                    backend,
                    format!("DROP DATABASE IF EXISTS {name}"),
                ))
                .await
                .expect("drop test database");
        }
    }

    #[test]
    fn duplicate_database_sqlstate_is_concurrent_success() {
        assert!(is_duplicate_database(&DbErr::Custom(
            "error: 42P04 duplicate_database".into()
        )));
        assert!(is_duplicate_database(&DbErr::Custom(
            "duplicate key value violates unique constraint \"pg_database_datname_index\"".into()
        )));
        assert!(!is_duplicate_database(&DbErr::Custom(
            "could not create isolated plugin database".into()
        )));
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_concurrent_first_open_treats_42p04_as_success() {
        let url = postgres_test_url();
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let name = format!("pb_race_{suffix}");
        let name = name.chars().take(63).collect::<String>();
        let url_a = url.clone();
        let url_b = url.clone();
        let name_a = name.clone();
        let name_b = name.clone();
        let (a, b) = tokio::join!(open_binding(&url_a, &name_a), open_binding(&url_b, &name_b));
        a.expect("first concurrent open");
        b.expect("second concurrent open treats 42P04 as success");
        let admin = Database::connect(url.as_str()).await.expect("admin");
        let backend = admin.get_database_backend();
        admin
            .execute_raw(Statement::from_string(
                backend,
                format!("DROP DATABASE IF EXISTS {name}"),
            ))
            .await
            .expect("drop race database");
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_drop_binding_reopen_is_empty() {
        let url = postgres_test_url();
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let name = format!("pb_drop_{suffix}");
        let name = name.chars().take(63).collect::<String>();
        let db = open_binding(&url, &name).await.expect("open");
        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            "CREATE TABLE notes (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)".to_string(),
        ))
        .await
        .expect("create");
        drop(db);
        drop_binding(&url, &name).await.expect("physical drop");
        let reopened = open_binding(&url, &name).await.expect("reopen after drop");
        let rows = reopened
            .query_all_raw(Statement::from_string(
                reopened.get_database_backend(),
                "SELECT to_regclass('public.notes') IS NOT NULL AS present".to_string(),
            ))
            .await
            .expect("probe");
        let present: bool = rows[0].try_get("", "present").unwrap_or(true);
        assert!(!present, "reopened binding must not keep the dropped table");
        drop(reopened);
        drop_binding(&url, &name).await.expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_binding_latin1_encoding_fails_readiness() {
        let url = postgres_test_url();
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let name = format!("pb_latin1_{suffix}");
        let name = name.chars().take(63).collect::<String>();
        let admin = Database::connect(url.as_str()).await.expect("admin");
        let backend = admin.get_database_backend();
        admin
            .execute_raw(Statement::from_string(
                backend,
                format!(
                    "CREATE DATABASE {name} ENCODING 'LATIN1' LC_COLLATE 'C' LC_CTYPE 'C' \
                     TEMPLATE template0"
                ),
            ))
            .await
            .expect("create latin1 binding database");
        drop(admin);
        let existing = open_binding_existing(&url, &name)
            .await
            .expect_err("latin1 binding must fail TEXT readiness");
        assert!(
            existing.to_string().contains("UTF8") || existing.to_string().contains("encoding"),
            "open_binding_existing: {existing}"
        );
        let created = open_binding(&url, &name)
            .await
            .expect_err("existing latin1 binding must fail TEXT readiness");
        assert!(
            created.to_string().contains("UTF8") || created.to_string().contains("encoding"),
            "open_binding: {created}"
        );
        drop_binding(&url, &name).await.expect("cleanup latin1");
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
    async fn postgres_binding_utf8_session_passes_readiness() {
        let url = postgres_test_url();
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let name = format!("pb_utf8_{suffix}");
        let name = name.chars().take(63).collect::<String>();
        let db = open_binding(&url, &name).await.expect("utf8 binding");
        let server = scalar_text(&db, "SHOW server_encoding")
            .await
            .expect("server_encoding");
        let client = scalar_text(&db, "SHOW client_encoding")
            .await
            .expect("client_encoding");
        assert!(is_utf8_encoding(&server), "server_encoding={server}");
        assert!(is_utf8_encoding(&client), "client_encoding={client}");
        drop(db);
        drop_binding(&url, &name).await.expect("cleanup utf8");
    }

    #[test]
    fn ci_postgres_matrix_covers_min_major_and_nothing_older() {
        let yml = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../.github/workflows/ci.yml"
        ));
        let jobs = yml
            .split("postgres-jobs:")
            .nth(1)
            .expect("ci.yml postgres-jobs");
        let job = jobs.split("ci-gate:").next().expect("postgres-jobs body");
        let line = job
            .lines()
            .find(|l| l.trim_start().starts_with("postgres:"))
            .expect("matrix postgres list");
        let mut majors = Vec::new();
        for part in line.split(|c: char| !c.is_ascii_digit()) {
            if part.is_empty() {
                continue;
            }
            majors.push(part.parse::<u32>().expect("postgres major"));
        }
        assert!(
            majors.contains(&MIN_POSTGRES_MAJOR),
            "CI must include postgres:{MIN_POSTGRES_MAJOR}: {majors:?}"
        );
        assert!(
            majors.iter().all(|n| *n >= MIN_POSTGRES_MAJOR),
            "CI postgres images must be >= {MIN_POSTGRES_MAJOR}: {majors:?}"
        );
        assert!(
            majors.contains(&17) && majors.contains(&18),
            "CI must matrix current supported majors: {majors:?}"
        );
        assert!(
            !majors.contains(&19),
            "postgres 19 was beta-only at implement time; add it after GA: {majors:?}"
        );
    }
}
