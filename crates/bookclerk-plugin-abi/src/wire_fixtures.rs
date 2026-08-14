//! Golden wire JSON fixtures (`fixtures/wire/`) — camelCase ABI DTOs.

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use std::collections::BTreeSet;

    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use serde_json::Value;

    use crate::{
        DbConnectParams, ExecResultDto, FetchTitleParams, LoginParams, LoginResultDto, PutParams,
        ScanParams, ScanSummaryDto,
    };

    fn load(name: &str) -> Value {
        let path = format!("{}/fixtures/wire/{name}", env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    /// Object keys must be camelCase: no `_` in keys, enforced recursively on
    /// fixture JSON. Opaque SQL row maps are not present in these goldens.
    ///
    /// # Panics
    ///
    /// Panics when a fixture key contains `_` or nested values violate the rule.
    fn assert_camel_case_keys(value: &Value, path: &str) {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    assert!(
                        !k.contains('_'),
                        "{path}: key `{k}` must be camelCase (no underscores)"
                    );
                    assert_camel_case_keys(v, &format!("{path}.{k}"));
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    assert_camel_case_keys(v, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }

    fn object_keys(value: &Value) -> BTreeSet<String> {
        value
            .as_object()
            .expect("expected JSON object")
            .keys()
            .cloned()
            .collect()
    }

    fn roundtrip_fixture<T>(name: &str)
    where
        T: DeserializeOwned + Serialize,
    {
        let fixture = load(name);
        assert_camel_case_keys(&fixture, name);

        let dto: T = serde_json::from_value(fixture.clone())
            .unwrap_or_else(|e| panic!("deserialize {name}: {e}"));
        let again = serde_json::to_value(&dto).unwrap_or_else(|e| panic!("serialize {name}: {e}"));
        assert_camel_case_keys(&again, &format!("{name} (roundtrip)"));

        // Fixture keys must survive the round trip (extra null Option fields are OK).
        let fixture_keys = object_keys(&fixture);
        let again_keys = object_keys(&again);
        assert!(
            fixture_keys.is_subset(&again_keys),
            "{name}: fixture keys missing after roundtrip\n  fixture: {fixture_keys:?}\n  again: {again_keys:?}"
        );

        // Multi-word snake_case aliases must be absent on the wire.
        for snake in [
            "plugin_data_dir",
            "callback_ipc",
            "account_id",
            "last_insert_id",
            "force_path_style",
            "source_config",
            "data_base64",
            "page_size",
            "scan_enabled",
            "books_upserted",
            "sqlite_path",
            "rows_affected",
            "access_key_id",
            "secret_access_key",
            "session_token",
            "content_type",
            "content_length",
            "title_id",
            "cache_dir",
            "import_episodes",
            "import_plus_titles",
        ] {
            assert!(
                again.get(snake).is_none(),
                "{name}: unexpected snake_case key `{snake}` after roundtrip"
            );
        }
    }

    #[test]
    fn login_request_roundtrip() {
        roundtrip_fixture::<LoginParams>("login.request.json");
    }

    #[test]
    fn login_result_roundtrip() {
        roundtrip_fixture::<LoginResultDto>("login.result.json");
        let v = load("login.result.json");
        assert!(v["account"].get("accountId").is_some());
        assert!(v["account"].get("account_id").is_none());
        assert!(v["account"].get("scanEnabled").is_some());
    }

    #[test]
    fn scan_request_roundtrip() {
        roundtrip_fixture::<ScanParams>("scan.request.json");
    }

    #[test]
    fn scan_result_roundtrip() {
        roundtrip_fixture::<ScanSummaryDto>("scan.result.json");
        let v = load("scan.result.json");
        assert!(v.get("booksUpserted").is_some());
        assert!(v.get("books_upserted").is_none());
    }

    #[test]
    fn fetch_title_request_roundtrip() {
        roundtrip_fixture::<FetchTitleParams>("fetchTitle.request.json");
        let v = load("fetchTitle.request.json");
        assert!(v.get("sourceConfig").is_some());
        assert!(v.get("source_config").is_none());
    }

    #[test]
    fn put_s3_request_roundtrip() {
        roundtrip_fixture::<PutParams>("put.s3.request.json");
        let v = load("put.s3.request.json");
        assert_eq!(v["forcePathStyle"], true);
        assert!(v.get("force_path_style").is_none());
        assert!(v["credentials"].get("accessKeyId").is_some());
    }

    #[test]
    fn db_connect_sqlite_roundtrip() {
        roundtrip_fixture::<DbConnectParams>("dbConnect.sqlite.json");
        let v = load("dbConnect.sqlite.json");
        assert_eq!(v["backend"], "sqlite");
        assert!(v.get("pluginDataDir").is_some());
        assert!(v.get("sqlitePath").is_some());
        assert!(v.get("plugin_data_dir").is_none());
        assert!(v.get("sqlite_path").is_none());
    }

    #[test]
    fn db_execute_result_roundtrip() {
        roundtrip_fixture::<ExecResultDto>("dbExecute.result.json");
        let v = load("dbExecute.result.json");
        assert!(v.get("lastInsertId").is_some());
        assert!(v.get("rowsAffected").is_some());
        assert!(v.get("last_insert_id").is_none());
        assert!(v.get("rows_affected").is_none());
    }

    #[test]
    fn fixtures_directory_lists_required_goldens() {
        let dir = format!("{}/fixtures/wire", env!("CARGO_MANIFEST_DIR"));
        let names: BTreeSet<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json"))
            .collect();
        for required in [
            "login.request.json",
            "login.result.json",
            "scan.request.json",
            "scan.result.json",
            "fetchTitle.request.json",
            "put.s3.request.json",
            "dbConnect.sqlite.json",
            "dbExecute.result.json",
        ] {
            assert!(
                names.contains(required),
                "missing required golden fixture: {required}"
            );
        }
    }
}
