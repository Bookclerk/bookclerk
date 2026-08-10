//! Workers RPC method names (camelCase).

/// All known method names for discovery / docs.
pub const METHOD_NAMES: &[&str] = &[
    handshake::NAME,
    shutdown::NAME,
    health::NAME,
    diagnose::NAME,
    start::NAME,
    on_event::NAME,
    poll_events::NAME,
    scan_library::NAME,
    sync_listening::NAME,
    authenticate_user::NAME,
    cli_describe::NAME,
    cli_invoke::NAME,
    login::NAME,
    login_start::NAME,
    login_complete::NAME,
    credentials_update::NAME,
    scan::NAME,
    fetch_title::NAME,
    search_catalog::NAME,
    expand_candidates::NAME,
    purchase_hint::NAME,
    list_deals::NAME,
    list_accounts::NAME,
    catalog_detail::NAME,
    put::NAME,
    put_file::NAME,
    get::NAME,
    exists::NAME,
    list::NAME,
    probe::NAME,
    copy::NAME,
    delete::NAME,
    touch_file::NAME,
    db_connect::NAME,
    db_ping::NAME,
    db_query::NAME,
    db_execute::NAME,
];

pub mod handshake {
    pub const NAME: &str = "handshake";
}
pub mod shutdown {
    pub const NAME: &str = "shutdown";
}
pub mod health {
    pub const NAME: &str = "health";
}
pub mod diagnose {
    pub const NAME: &str = "diagnose";
}
pub mod start {
    pub const NAME: &str = "start";
}
pub mod on_event {
    pub const NAME: &str = "onEvent";
}
pub mod poll_events {
    pub const NAME: &str = "pollEvents";
}
pub mod scan_library {
    pub const NAME: &str = "scanLibrary";
}
pub mod sync_listening {
    pub const NAME: &str = "syncListening";
}
pub mod authenticate_user {
    pub const NAME: &str = "authenticateUser";
}
pub mod cli_describe {
    pub const NAME: &str = "cliDescribe";
}
pub mod cli_invoke {
    pub const NAME: &str = "cliInvoke";
}
pub mod login {
    pub const NAME: &str = "login";
}
pub mod login_start {
    pub const NAME: &str = "loginStart";
}
pub mod login_complete {
    pub const NAME: &str = "loginComplete";
}
pub mod credentials_update {
    pub const NAME: &str = "credentialsUpdate";
}
pub mod scan {
    pub const NAME: &str = "scan";
}
pub mod fetch_title {
    pub const NAME: &str = "fetchTitle";
}
pub mod search_catalog {
    pub const NAME: &str = "searchCatalog";
}
pub mod expand_candidates {
    pub const NAME: &str = "expandCandidates";
}
pub mod purchase_hint {
    pub const NAME: &str = "purchaseHint";
}
pub mod list_deals {
    pub const NAME: &str = "listDeals";
}
pub mod list_accounts {
    pub const NAME: &str = "listAccounts";
}
pub mod catalog_detail {
    pub const NAME: &str = "catalogDetail";
}
pub mod put {
    pub const NAME: &str = "put";
}
pub mod put_file {
    pub const NAME: &str = "putFile";
}
pub mod get {
    pub const NAME: &str = "get";
}
pub mod exists {
    pub const NAME: &str = "exists";
}
pub mod list {
    pub const NAME: &str = "list";
}
pub mod probe {
    pub const NAME: &str = "probe";
}
pub mod copy {
    pub const NAME: &str = "copy";
}
pub mod delete {
    pub const NAME: &str = "delete";
}
pub mod touch_file {
    pub const NAME: &str = "touchFile";
}
pub mod db_connect {
    pub const NAME: &str = "dbConnect";
}
pub mod db_ping {
    pub const NAME: &str = "dbPing";
}
pub mod db_query {
    pub const NAME: &str = "dbQuery";
}
pub mod db_execute {
    pub const NAME: &str = "dbExecute";
}

/// Flat constants matching historical `protocol::methods` usage.
pub mod names {
    pub use super::authenticate_user::NAME as AUTHENTICATE_USER;
    pub use super::catalog_detail::NAME as CATALOG_DETAIL;
    pub use super::cli_describe::NAME as CLI_DESCRIBE;
    pub use super::cli_invoke::NAME as CLI_INVOKE;
    pub use super::copy::NAME as COPY;
    pub use super::credentials_update::NAME as CREDENTIALS_UPDATE;
    pub use super::db_connect::NAME as DB_CONNECT;
    pub use super::db_execute::NAME as DB_EXECUTE;
    pub use super::db_ping::NAME as DB_PING;
    pub use super::db_query::NAME as DB_QUERY;
    pub use super::delete::NAME as DELETE;
    pub use super::diagnose::NAME as DIAGNOSE;
    pub use super::exists::NAME as EXISTS;
    pub use super::expand_candidates::NAME as EXPAND_CANDIDATES;
    pub use super::fetch_title::NAME as FETCH_TITLE;
    pub use super::get::NAME as GET;
    pub use super::handshake::NAME as HANDSHAKE;
    pub use super::health::NAME as HEALTH;
    pub use super::list::NAME as LIST;
    pub use super::list_accounts::NAME as LIST_ACCOUNTS;
    pub use super::list_deals::NAME as LIST_DEALS;
    pub use super::login::NAME as LOGIN;
    pub use super::login_complete::NAME as LOGIN_COMPLETE;
    pub use super::login_start::NAME as LOGIN_START;
    pub use super::on_event::NAME as ON_EVENT;
    pub use super::poll_events::NAME as EVENT_POLL;
    pub use super::probe::NAME as PROBE;
    pub use super::purchase_hint::NAME as PURCHASE_HINT;
    pub use super::put::NAME as PUT;
    pub use super::put_file::NAME as PUT_FILE;
    pub use super::scan::NAME as SCAN;
    pub use super::scan_library::NAME as SCAN_LIBRARY;
    pub use super::search_catalog::NAME as SEARCH_CATALOG;
    pub use super::shutdown::NAME as SHUTDOWN;
    pub use super::start::NAME as START;
    pub use super::sync_listening::NAME as SYNC_LISTENING;
    pub use super::touch_file::NAME as TOUCH_FILE;
}
