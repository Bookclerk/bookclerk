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
    bootstrap_for, capabilities_for, guest_assert_restore_constraints,
    guest_assert_restore_constraints_on, guest_assert_restore_constraints_on_txn, guest_begin,
    guest_begin_isolated, guest_begin_on, guest_begin_on_isolated, guest_bootstrap,
    guest_capabilities, guest_commit, guest_drop_user_relations, guest_drop_user_relations_on,
    guest_drop_user_relations_on_txn, guest_execute, guest_execute_atomic, guest_execute_atomic_on,
    guest_execute_atomic_on_txn, guest_execute_request, guest_execute_request_on,
    guest_export_identity, guest_export_identity_on, guest_export_identity_on_txn,
    guest_import_identity, guest_import_identity_on, guest_import_identity_on_txn,
    guest_list_user_relations, guest_list_user_relations_on, guest_list_user_relations_on_txn,
    guest_ping, guest_prepare_unit_restore, guest_prepare_unit_restore_on,
    guest_prepare_unit_restore_on_txn, guest_query, guest_query_page, guest_rollback, row_to_dto,
    set_connection,
};
pub use sql::{guest_sql, GuestStatement};
