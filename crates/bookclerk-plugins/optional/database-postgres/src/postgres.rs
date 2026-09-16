//! PostgreSQL engine for the database plugin (SeaORM sqlx-postgres).

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, RuntimeErr, Statement};
use std::ops::Deref;

/// Open Postgres with a host-mediated connection URL (ping only; host applies schema).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn open(url: &str) -> std::result::Result<DatabaseConnection, DbErr> {
    let db = Database::connect(url).await?;
    db.ping().await?;
    tracing::debug!(plugin = "postgres", "opened library database");
    Ok(db)
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
/// role needs `CREATEDB`), or the binding connection cannot ping.
pub async fn open_binding(
    url: &str,
    database: &str,
) -> std::result::Result<DatabaseConnection, DbErr> {
    if !binding_database_name_ok(database) {
        return Err(DbErr::Custom(format!(
            "invalid binding database name `{database}`"
        )));
    }
    let admin = Database::connect(url).await?;
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
    let db = Database::connect(postgres_url_with_database(url, database)).await?;
    db.ping().await?;
    tracing::debug!(plugin = "postgres", database, "opened binding database");
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
    let admin = Database::connect(url).await?;
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
}
