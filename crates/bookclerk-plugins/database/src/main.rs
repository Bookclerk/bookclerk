//! External library database plugin for Bookclerk (SQLite, D1, Postgres).

use bookclerk_plugin_sdk::{
    methods, DbConnectParams, HandshakeResult, HealthDto, PluginGuest, StatementDto,
    PLUGIN_API_VERSION,
};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plugin_id = std::env::var("BOOKCLERK_PLUGIN_ID").unwrap_or_else(|_| "sqlite".into());
    PluginGuest::serve(move |method, params| {
        let plugin_id = plugin_id.clone();
        async move {
            match method.as_str() {
                methods::HANDSHAKE => Ok(serde_json::to_value(HandshakeResult {
                    api_version: PLUGIN_API_VERSION,
                    id: plugin_id.clone(),
                    kind: "database".into(),
                    display_name: Some(display_name(&plugin_id)),
                    capabilities: vec![
                        "health".into(),
                        "diagnose".into(),
                        "db.connect".into(),
                        "db.ping".into(),
                        "db.query".into(),
                        "db.execute".into(),
                    ],
                    portal_auth_mode: None,
                    password_env_var: None,
                    aliases: vec![],
                    sort_key: Some(5),
                    brand: None,
                    config_options: vec![],
                    cli: None,
                })
                .unwrap()),
                methods::HEALTH => Ok(serde_json::to_value(HealthDto {
                    id: plugin_id.clone(),
                    enabled: true,
                    ok: true,
                    detail: Some(format!("{} database plugin ready", plugin_id)),
                })
                .unwrap()),
                methods::DIAGNOSE => {
                    Ok(json!([format!("{plugin_id} database plugin diagnose: ok")]))
                }
                methods::DB_CONNECT => {
                    let p: DbConnectParams = serde_json::from_value(params)
                        .map_err(|e| format!("db.connect params: {e}"))?;
                    let result = bookclerk_plugin_database::guest::guest_connect(p).await?;
                    Ok(serde_json::to_value(result).unwrap())
                }
                methods::DB_PING => bookclerk_plugin_database::guest::guest_ping()
                    .await
                    .map(|_| serde_json::Value::Null),
                methods::DB_QUERY => {
                    let p: StatementDto = serde_json::from_value(params)
                        .map_err(|e| format!("db.query params: {e}"))?;
                    let dto = bookclerk_plugin_database::guest::guest_query(p).await?;
                    Ok(serde_json::to_value(dto).unwrap())
                }
                methods::DB_EXECUTE => {
                    let p: StatementDto = serde_json::from_value(params)
                        .map_err(|e| format!("db.execute params: {e}"))?;
                    let dto = bookclerk_plugin_database::guest::guest_execute(p).await?;
                    Ok(serde_json::to_value(dto).unwrap())
                }
                other => Err(format!("unsupported method `{other}`")),
            }
        }
    })
    .await?;
    Ok(())
}

fn display_name(id: &str) -> String {
    match id.to_ascii_lowercase().as_str() {
        "d1" => "Cloudflare D1".into(),
        "postgres" => "PostgreSQL".into(),
        _ => "SQLite".into(),
    }
}
