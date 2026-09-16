//! Positive author-surface fixture.
//!
//! Everything a third-party database adapter author needs must compile with
//! the documented feature set (`bookclerk-plugin-sdk` + `db`), without the
//! abi `host` feature anywhere in the graph.

pub use bookclerk_plugin_sdk::database_adapter::{errors, migrate};
pub use bookclerk_plugin_sdk::{
    AdapterDatabaseSession, Database, DatabaseBinding, DbBootstrap, DbCapabilities, ExecuteReply,
    ExecuteRequest, PluginError,
};

/// Type-level proof the adapter traits and typed execute DTOs are visible.
pub fn author_surface(
    _database: &dyn Database,
    _session: &dyn AdapterDatabaseSession,
    binding: &DatabaseBinding,
    caps: &DbCapabilities,
    bootstrap: &DbBootstrap,
    request: &ExecuteRequest,
    reply: &ExecuteReply,
) -> (u32, usize, usize, usize) {
    let _ = binding;
    (
        caps.max_binds,
        bootstrap.sql_family.len(),
        request.statements.len(),
        reply.statements.len(),
    )
}

/// The `database_adapter` helper modules stay on the author surface.
pub fn author_helpers(engine_error: &str) -> PluginError {
    let _ = migrate::split_sql_statements("SELECT 1;");
    let _ = migrate::typed_null(Some("INTEGER"), "id");
    errors::plugin_error_from_engine(engine_error)
}
