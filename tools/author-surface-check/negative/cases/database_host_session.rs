// `Database::host_session` is host-private machinery. It must not appear on
// the author-facing `Database` trait, even with the documented `db` feature
// (no dependency in the author graph may enable the abi `host` feature).
pub fn probe(db: &dyn bookclerk_plugin_sdk::Database) {
    let _ = db.host_session();
}
