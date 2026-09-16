//! First-party database guest internals (SeaORM session workers).
//!
//! Third-party database plugin authors should depend on
//! [`bookclerk_plugin_sdk::database_adapter`] only. Platform SQLite / Postgres /
//! D1 guests use this crate for host-mediated session and transaction workers.

mod host_session;
mod session;
mod sql;

pub use host_session::{
    host_session, host_session_on, BoundGuestHostAdapterSession, GuestHostAdapterSession,
};
pub use session::{
    bootstrap_for, capabilities_for, guest_begin, guest_begin_on, guest_bootstrap,
    guest_capabilities, guest_commit, guest_execute, guest_execute_atomic, guest_execute_atomic_on,
    guest_execute_atomic_on_txn, guest_execute_request, guest_execute_request_on, guest_ping,
    guest_query, guest_query_page, guest_rollback, row_to_dto, set_connection,
};
pub use sql::{guest_sql, GuestStatement};
