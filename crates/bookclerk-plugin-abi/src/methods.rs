//! Role capability method names (camelCase on the wire).
//!
//! Each submodule exposes a single [`NAME`](health::NAME) constant equal to
//! the corresponding `ContentSource` / `Integration` / `Plugin` method in
//! `schema/plugin.capnp`. Prefer these constants over string literals when
//! dispatching or documenting capabilities.
//!
//! [`METHOD_NAMES`] enumerates every known method for discovery / schema
//! drift tests. Flat `UPPER_SNAKE` aliases live under [`names`].

/// All known capability method names for discovery, docs, and schema checks.
///
/// Order is stable for readability; `scripts/gen-plugin-abi.py --check`
/// verifies this list against `schema/plugin.capnp`.
pub const METHOD_NAMES: &[&str] = &[
    shutdown::NAME,
    health::NAME,
    diagnose::NAME,
    start::NAME,
    stop::NAME,
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
    scan::NAME,
    fetch_title::NAME,
    search_catalog::NAME,
    expand_candidates::NAME,
    purchase_hint::NAME,
    list_deals::NAME,
    list_accounts::NAME,
    catalog_detail::NAME,
    put::NAME,
    get::NAME,
    exists::NAME,
    list::NAME,
    copy::NAME,
    delete::NAME,
];

/// Graceful guest teardown before the host closes the transport.
pub mod shutdown {
    /// Wire method name `"shutdown"`.
    pub const NAME: &str = "shutdown";
}

/// Connectivity / configuration probe.
///
/// Result: [`crate::types::HealthResult`] or host-adapter [`crate::types::HealthDto`].
pub mod health {
    /// Wire method name `"health"`.
    pub const NAME: &str = "health";
}

/// Human-readable diagnostic lines for `bookclerk plugins diagnose`.
///
/// Result: [`crate::types::DiagnoseResult`].
pub mod diagnose {
    /// Wire method name `"diagnose"`.
    pub const NAME: &str = "diagnose";
}

/// Start background watchers (integration capability `start`).
pub mod start {
    /// Wire method name `"start"`.
    pub const NAME: &str = "start";
}

/// Stop background watchers started by [`start`] (Cap'n Proto `Integration.stop`).
pub mod stop {
    /// Wire method name `"stop"`.
    pub const NAME: &str = "stop";
}

/// Deliver a host→plugin domain event (Cap'n Proto `Integration.onEvent`).
pub mod on_event {
    /// Wire method name `"onEvent"`.
    pub const NAME: &str = "onEvent";
}

/// Poll for plugin-observed external users after [`start`].
///
/// Result: [`crate::kind::EventPollResultDto`].
pub mod poll_events {
    /// Wire method name `"pollEvents"`.
    pub const NAME: &str = "pollEvents";
}

/// Trigger an integration library scan against a remote server (e.g. ABS).
///
/// Params: [`crate::kind::ScanLibraryParams`].
pub mod scan_library {
    /// Wire method name `"scanLibrary"`.
    pub const NAME: &str = "scanLibrary";
}

/// Pull listening-progress snapshots; host upserts tagged with the plugin id.
///
/// Result: [`crate::kind::SyncListeningResultDto`].
pub mod sync_listening {
    /// Wire method name `"syncListening"`.
    pub const NAME: &str = "syncListening";
}

/// Validate a username/password against an integration and return an external user.
///
/// Params: [`crate::kind::AuthenticateUserParams`].
pub mod authenticate_user {
    /// Wire method name `"authenticateUser"`.
    pub const NAME: &str = "authenticateUser";
}

/// Return the guest's declared CLI schema ([`crate::types::CliSchema`]).
pub mod cli_describe {
    /// Wire method name `"cliDescribe"`.
    pub const NAME: &str = "cliDescribe";
}

/// Invoke a declared plugin CLI command.
///
/// Params: [`crate::types::CliInvokeParams`]. Result: [`crate::types::CliInvokeResult`].
pub mod cli_invoke {
    /// Wire method name `"cliInvoke"`.
    pub const NAME: &str = "cliInvoke";
}

/// Password-style source login; host seals returned credentials.
///
/// Params: [`crate::kind::LoginParams`]. Result: [`crate::kind::LoginResultDto`].
pub mod login {
    /// Wire method name `"login"`.
    pub const NAME: &str = "login";
}

/// Begin interactive OAuth (returns browser URL + session id).
///
/// Params: [`crate::kind::LoginStartParams`]. Result: [`crate::kind::LoginStartResultDto`].
pub mod login_start {
    /// Wire method name `"loginStart"`.
    pub const NAME: &str = "loginStart";
}

/// Finish interactive OAuth after the operator completes the browser flow.
///
/// Params: [`crate::kind::LoginCompleteParams`]. Result: [`crate::kind::LoginResultDto`].
pub mod login_complete {
    /// Wire method name `"loginComplete"`.
    pub const NAME: &str = "loginComplete";
}

/// Scan a source storefront library; host upserts returned [`crate::kind::ScanBookDto`] rows.
///
/// Params: [`crate::kind::ScanParams`]. Result: [`crate::kind::ScanSummaryDto`].
pub mod scan {
    /// Wire method name `"scan"`.
    pub const NAME: &str = "scan";
}

/// Download/decrypt one title into the host cache directory.
///
/// Params: [`crate::kind::FetchTitleParams`]. Result: [`crate::kind::SourceFetchDto`].
pub mod fetch_title {
    /// Wire method name `"fetchTitle"`.
    pub const NAME: &str = "fetchTitle";
}

/// Search a storefront catalog for purchase / discovery UIs.
///
/// Params: [`crate::kind::SearchCatalogParams`]. Result: list of [`crate::kind::CatalogHitDto`].
pub mod search_catalog {
    /// Wire method name `"searchCatalog"`.
    pub const NAME: &str = "searchCatalog";
}

/// Expand related catalog candidates from a seed title.
///
/// Params: [`crate::kind::ExpandCandidatesParams`]. Result: list of [`crate::kind::CatalogHitDto`].
pub mod expand_candidates {
    /// Wire method name `"expandCandidates"`.
    pub const NAME: &str = "expandCandidates";
}

/// Resolve a purchase URL / price hint for a catalog product.
///
/// Params: [`crate::kind::PurchaseHintParams`]. Result: [`crate::kind::PurchaseHintDto`].
pub mod purchase_hint {
    /// Wire method name `"purchaseHint"`.
    pub const NAME: &str = "purchaseHint";
}

/// List current storefront deals / promotions.
///
/// Params: [`crate::kind::ListDealsParams`].
pub mod list_deals {
    /// Wire method name `"listDeals"`.
    pub const NAME: &str = "listDeals";
}

/// List source accounts known to the guest (host usually prefers DB rows).
///
/// Params: [`crate::kind::ListAccountsParams`].
pub mod list_accounts {
    /// Wire method name `"listAccounts"`.
    pub const NAME: &str = "listAccounts";
}

/// Fetch rich catalog detail for one product id.
///
/// Params: [`crate::kind::CatalogDetailParams`]. Result: [`crate::kind::CatalogHitDto`].
pub mod catalog_detail {
    /// Wire method name `"catalogDetail"`.
    pub const NAME: &str = "catalogDetail";
}

/// Put a small object (cover/sidecar) into an output destination.
///
/// Params: [`crate::kind::OutputPutParams`].
pub mod put {
    /// Wire method name `"put"`.
    pub const NAME: &str = "put";
}

/// Read a small object from an output destination.
///
/// Params: [`crate::kind::OutputGetParams`]. Result: [`crate::kind::GetResultDto`].
pub mod get {
    /// Wire method name `"get"`.
    pub const NAME: &str = "get";
}

/// Test whether an object key exists in an output destination.
///
/// Params: [`crate::kind::OutputKeyParams`]. Result: [`crate::kind::ExistsResultDto`].
pub mod exists {
    /// Wire method name `"exists"`.
    pub const NAME: &str = "exists";
}

/// List object keys under a prefix in an output destination.
///
/// Params: [`crate::kind::OutputListParams`]. Result: list of [`crate::kind::ObjectInfoDto`].
pub mod list {
    /// Wire method name `"list"`.
    pub const NAME: &str = "list";
}

/// Copy an object within an output destination.
///
/// Params: [`crate::kind::OutputCopyParams`].
pub mod copy {
    /// Wire method name `"copy"`.
    pub const NAME: &str = "copy";
}

/// Delete an object key from an output destination.
///
/// Params: [`crate::kind::OutputKeyParams`].
pub mod delete {
    /// Wire method name `"delete"`.
    pub const NAME: &str = "delete";
}

/// Flat `UPPER_SNAKE` aliases matching historical `protocol::methods` usage.
///
/// Prefer the namespaced modules (`login_start::NAME`) in new code; these
/// constants remain for call sites that imported a single `methods` prelude.
pub mod names {
    /// Alias of [`super::authenticate_user::NAME`] (`"authenticateUser"`).
    pub use super::authenticate_user::NAME as AUTHENTICATE_USER;
    /// Alias of [`super::catalog_detail::NAME`] (`"catalogDetail"`).
    pub use super::catalog_detail::NAME as CATALOG_DETAIL;
    /// Alias of [`super::cli_describe::NAME`] (`"cliDescribe"`).
    pub use super::cli_describe::NAME as CLI_DESCRIBE;
    /// Alias of [`super::cli_invoke::NAME`] (`"cliInvoke"`).
    pub use super::cli_invoke::NAME as CLI_INVOKE;
    /// Alias of [`super::copy::NAME`] (`"copy"`).
    pub use super::copy::NAME as COPY;
    /// Alias of [`super::delete::NAME`] (`"delete"`).
    pub use super::delete::NAME as DELETE;
    /// Alias of [`super::diagnose::NAME`] (`"diagnose"`).
    pub use super::diagnose::NAME as DIAGNOSE;
    /// Alias of [`super::exists::NAME`] (`"exists"`).
    pub use super::exists::NAME as EXISTS;
    /// Alias of [`super::expand_candidates::NAME`] (`"expandCandidates"`).
    pub use super::expand_candidates::NAME as EXPAND_CANDIDATES;
    /// Alias of [`super::fetch_title::NAME`] (`"fetchTitle"`).
    pub use super::fetch_title::NAME as FETCH_TITLE;
    /// Alias of [`super::get::NAME`] (`"get"`).
    pub use super::get::NAME as GET;
    /// Alias of [`super::health::NAME`] (`"health"`).
    pub use super::health::NAME as HEALTH;
    /// Alias of [`super::list::NAME`] (`"list"`).
    pub use super::list::NAME as LIST;
    /// Alias of [`super::list_accounts::NAME`] (`"listAccounts"`).
    pub use super::list_accounts::NAME as LIST_ACCOUNTS;
    /// Alias of [`super::list_deals::NAME`] (`"listDeals"`).
    pub use super::list_deals::NAME as LIST_DEALS;
    /// Alias of [`super::login::NAME`] (`"login"`).
    pub use super::login::NAME as LOGIN;
    /// Alias of [`super::login_complete::NAME`] (`"loginComplete"`).
    pub use super::login_complete::NAME as LOGIN_COMPLETE;
    /// Alias of [`super::login_start::NAME`] (`"loginStart"`).
    pub use super::login_start::NAME as LOGIN_START;
    /// Alias of [`super::on_event::NAME`] (`"onEvent"`).
    pub use super::on_event::NAME as ON_EVENT;
    /// Alias of [`super::poll_events::NAME`] (`"pollEvents"`).
    pub use super::poll_events::NAME as EVENT_POLL;
    /// Alias of [`super::purchase_hint::NAME`] (`"purchaseHint"`).
    pub use super::purchase_hint::NAME as PURCHASE_HINT;
    /// Alias of [`super::put::NAME`] (`"put"`).
    pub use super::put::NAME as PUT;
    /// Alias of [`super::scan::NAME`] (`"scan"`).
    pub use super::scan::NAME as SCAN;
    /// Alias of [`super::scan_library::NAME`] (`"scanLibrary"`).
    pub use super::scan_library::NAME as SCAN_LIBRARY;
    /// Alias of [`super::search_catalog::NAME`] (`"searchCatalog"`).
    pub use super::search_catalog::NAME as SEARCH_CATALOG;
    /// Alias of [`super::shutdown::NAME`] (`"shutdown"`).
    pub use super::shutdown::NAME as SHUTDOWN;
    /// Alias of [`super::start::NAME`] (`"start"`).
    pub use super::start::NAME as START;
    /// Alias of [`super::stop::NAME`] (`"stop"`).
    pub use super::stop::NAME as STOP;
    /// Alias of [`super::sync_listening::NAME`] (`"syncListening"`).
    pub use super::sync_listening::NAME as SYNC_LISTENING;
}
