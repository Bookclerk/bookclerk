use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_library::entities::title_requests;
use bookclerk_plugin_sdk::{proxy_rows_from_dto, QueryResultDto};
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, DbBackend, DbErr, EntityTrait, ProxyDatabaseTrait,
    ProxyExecResult, ProxyRow, QueryFilter, QueryTrait, Statement,
};

#[tokio::test]
async fn title_request_select_survives_guest_dto_roundtrip() {
    let db = bookclerk_plugin_database::sqlite::open_memory()
        .await
        .unwrap();
    let store = bookclerk_library::LibraryStore::from_connection(db.clone());
    let created = store
        .create_title_request(&bookclerk_library::NewTitleRequest {
            uuid: None,
            identity_id: None,
            title: "T".into(),
            authors: Some("A".into()),
            asin: Some("B00X".into()),
            isbn: None,
            notes: None,
            status: bookclerk_library::RequestStatus::Open,
            work_id: None,
            work_key: "asin:B00X".into(),
            resolved_book_uuid: None,
            cover_url: Some("https://example.com/c.jpg".into()),
        })
        .await
        .unwrap();

    let stmt = title_requests::Entity::find().build(DbBackend::Sqlite);
    let rows = db.query_all_raw(stmt).await.unwrap();
    assert_eq!(rows.len(), 1);

    let mut dto_rows = Vec::new();
    for row in &rows {
        // Same helper the guest uses on the wire.
        dto_rows.push(bookclerk_plugin_database::guest::row_to_dto(row));
    }
    let dto = QueryResultDto { rows: dto_rows };
    assert_eq!(
        dto.rows[0].values.get("uuid").and_then(|v| v.as_str()),
        Some(created.uuid.as_str())
    );

    let proxy_rows = proxy_rows_from_dto(dto.rows);

    #[derive(Debug)]
    struct OnceProxy(Vec<ProxyRow>);
    #[async_trait]
    impl ProxyDatabaseTrait for OnceProxy {
        async fn query(&self, _: Statement) -> Result<Vec<ProxyRow>, DbErr> {
            Ok(self.0.clone())
        }
        async fn execute(&self, _: Statement) -> Result<ProxyExecResult, DbErr> {
            Ok(ProxyExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            })
        }
        async fn ping(&self) -> Result<(), DbErr> {
            Ok(())
        }
    }

    let proxied =
        Database::connect_proxy(DbBackend::Sqlite, Arc::new(Box::new(OnceProxy(proxy_rows))))
            .await
            .unwrap();
    let decoded = title_requests::Entity::find()
        .filter(title_requests::Column::Asin.eq("B00X"))
        .all(&proxied)
        .await
        .expect("decode after dto roundtrip");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].uuid, created.uuid);
    assert_eq!(
        decoded[0].cover_url.as_deref(),
        Some("https://example.com/c.jpg")
    );
}
