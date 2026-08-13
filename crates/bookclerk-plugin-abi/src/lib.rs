//! Authoritative Bookclerk plugin ABI (`api_version = 1`).
//!
//! # Audience
//!
//! - **Guest authors** — implement Workers RPC methods against these DTOs
//!   (via `bookclerk-plugin-sdk`, `@bookclerk/plugin-sdk`, or a language binding
//!   generated from the same schema).
//! - **Host / SDK maintainers** — deserialize stdio or workerd frames, seal
//!   credentials, and upsert library rows without depending on store-specific
//!   crates.
//!
//! Product narrative (jail, consent, install layout) lives in
//! [`docs/plugins.md`](https://github.com/bookclerk/bookclerk/blob/main/docs/plugins.md).
//! This crate is the typed wire contract only.
//!
//! # Schema
//!
//! The JSON Schema at [`schema/abi.json`](https://github.com/bookclerk/bookclerk/blob/main/crates/bookclerk-plugin-abi/schema/abi.json)
//! (also embedded as [`ABI_SCHEMA_JSON`]) is the canonical contract. Types here
//! are the Rust projection used by host and guest SDKs. Wire DTO fields
//! serialize as **camelCase** to match Workers RPC / TypeScript (`abi.json`
//! `$defs`). Method names on the wire are camelCase strings listed in
//! [`methods::METHOD_NAMES`] (for example `loginStart`, `fetchTitle`,
//! `dbConnect`).
//!
//! Install manifests validate against [`PLUGIN_TOML_SCHEMA_JSON`]
//! (`schema/plugin-toml.json`).
//!
//! # Versioning
//!
//! [`API_VERSION`] is currently `1`. Guests that advertise a different
//! `apiVersion` on [`types::HandshakeParams`] fail handshake cleanly. There is
//! no `protocol` key — only `api_version` / wire `apiVersion`. Bumping the
//! version requires a coordinated schema + SDK change; do not invent ad-hoc
//! fields outside `$defs`.
//!
//! # Modules
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`methods`] | Wire method name constants (`handshake`, `onEvent`, …) |
//! | [`types`] | Shared DTOs (handshake, health, CLI, stdio RPC frames) |
//! | [`kind`] | Kind-specific DTOs (source / integration / output) |
//! | [`db`] | Database-guest connect / query / execute DTOs |
//! | [`error`] | [`PluginError`] / [`PluginErrorCode`] |
//! | [`events`] | Host↔plugin typed event envelopes |

pub mod db;
pub mod error;
pub mod events;
pub mod kind;
pub mod methods;
pub mod types;

#[cfg(test)]
mod wire_fixtures;

pub use db::{
    atomic_status, DbAtomicParams, DbAtomicRequest, DbAtomicResult, DbAtomicTiming, DbBeginParams,
    DbBeginResult, DbConnectParams, DbConnectResult, DbTxnParams, ExecResultDto, ProxyRowDto,
    QueryResultDto, StatementDto,
};
pub use error::{PluginError, PluginErrorCode, Result};
pub use events::{HostToPluginEvent, PluginToHostEvent};
pub use kind::*;
pub use methods::METHOD_NAMES;
pub use types::*;

/// Negotiated Workers RPC API version for all guests (`1`).
///
/// Sent as wire field `apiVersion` on handshake params/results. Must match the
/// `const` in `schema/abi.json`.
pub const API_VERSION: u32 = 1;

/// Embedded bytes of `schema/abi.json` (CI and docs tooling can compare
/// generators against this exact string).
pub const ABI_SCHEMA_JSON: &str = include_str!("../schema/abi.json");

/// Embedded JSON Schema for install `plugin.toml` files (shared with language
/// SDK author tools and `bookclerk plugins` validation).
pub const PLUGIN_TOML_SCHEMA_JSON: &str = include_str!("../schema/plugin-toml.json");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_parses_as_json() {
        let v: serde_json::Value =
            serde_json::from_str(ABI_SCHEMA_JSON).expect("abi.json must be valid JSON");
        assert_eq!(v["title"], "BookclerkPluginAbi");
    }

    #[test]
    fn handshake_roundtrip_camel_case() {
        let hs = HandshakeResult {
            api_version: API_VERSION,
            id: "echo".into(),
            kind: "integration".into(),
            capabilities: vec!["health".into()],
            ..HandshakeResult::default()
        };
        let v = serde_json::to_value(&hs).unwrap();
        assert!(v.get("apiVersion").is_some());
        assert!(v.get("api_version").is_none());
        let back: HandshakeResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "echo");
    }

    #[test]
    fn method_names_match_schema() {
        let v: serde_json::Value = serde_json::from_str(ABI_SCHEMA_JSON).unwrap();
        let mut schema_names: Vec<&str> = v["properties"]["methods"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        let mut expected: Vec<&str> = METHOD_NAMES.to_vec();
        schema_names.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            schema_names, expected,
            "abi.json methods keys must match methods.rs METHOD_NAMES"
        );
    }

    #[test]
    fn db_atomic_params_use_camel_case_op_tag() {
        let params = DbAtomicParams::DeleteUser { user_id: 7 };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["op"], "deleteUser");
        assert_eq!(v["userId"], 7);
        let back: DbAtomicParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, params);

        let take = DbAtomicParams::TakeOidcRpState {
            state_hash: "abc".into(),
        };
        let tv = serde_json::to_value(&take).unwrap();
        assert_eq!(tv["op"], "takeOidcRpState");
        assert_eq!(tv["stateHash"], "abc");
        let take_back: DbAtomicParams = serde_json::from_value(tv).unwrap();
        assert_eq!(take_back, take);

        let req = DbAtomicRequest {
            operation_id: "op-1".into(),
            operation: take.clone(),
        };
        let rv = serde_json::to_value(&req).unwrap();
        assert_eq!(rv["operationId"], "op-1");
        assert_eq!(rv["operation"]["op"], "takeOidcRpState");

        let chal = DbAtomicParams::TakeWebauthnChallenge {
            challenge_id: "c1".into(),
            kind: "login".into(),
        };
        let cv = serde_json::to_value(&chal).unwrap();
        assert_eq!(cv["op"], "takeWebauthnChallenge");
        assert_eq!(cv["challengeId"], "c1");
        assert_eq!(cv["kind"], "login");

        let d1 = DbConnectResult::d1();
        let rv = serde_json::to_value(&d1).unwrap();
        assert_eq!(rv["dialect"], "sqlite");
        assert_eq!(rv["interactiveTxn"], false);
    }
}
