//! Bodies of a branch's store-connection routes, parsed sans IO:
//! `POST /api/config/join/connections` ("Add my connections to
//! *library*") and `POST /api/config/join/connections/disconnect`.
//!
//! The daemon reads each body into [`AddConnectionsBody`] or
//! [`DisconnectBody`] and turns it into a request here, so the routes, the
//! unit tests and the `connections_request` fuzz target all drive the same
//! code ([`parse_add_connections_request`], [`parse_disconnect_request`]).

use serde::Deserialize;

use super::cluster_links::from_object;
use crate::error::{LibraryError, Result};

/// Most accounts one "Add my connections" request names: as many as one
/// sealed account bundle carries.
pub const MAX_ADD_CONNECTIONS: usize = bookclerk_node_proto::MAX_BUNDLE_ACCOUNTS;

/// One store account, by storefront plugin key and store account id.
#[derive(Debug, Clone, PartialEq, Eq, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `AccountRefFields` enforces it.
#[serde(deny_unknown_fields)]
pub struct AccountRef {
    /// Provenance-qualified plugin key of the storefront, as sent.
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    pub plugin_key: String,
    /// Store account id, as sent.
    #[schema(example = "reader@example.com")]
    pub account_id: String,
}

/// [`AccountRef`]'s fields as JSON carries them; read only from a JSON
/// object ([`AccountRef`]'s `Deserialize`), never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountRefFields {
    /// See [`AccountRef`].
    plugin_key: String,
    /// See [`AccountRef`].
    account_id: String,
}

impl<'de> Deserialize<'de> for AccountRef {
    /// Reads the account from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: AccountRefFields =
            from_object(deserializer, "a JSON object with plugin_key and account_id")?;
        Ok(Self {
            plugin_key: fields.plugin_key,
            account_id: fields.account_id,
        })
    }
}

/// `POST /api/config/join/connections` body.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `AddConnectionsFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct AddConnectionsBody {
    /// The library (owner cluster id) to add the connections to.
    #[schema(example = "6d1f2e3a-4b5c-4d6e-8f7a-9b0c1d2e3f4a")]
    cluster_id: String,
    /// The person's accounts to add; 1 to 64.
    accounts: Vec<AccountRef>,
}

/// [`AddConnectionsBody`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddConnectionsFields {
    /// See [`AddConnectionsBody`].
    cluster_id: String,
    /// See [`AddConnectionsBody`].
    accounts: Vec<AccountRef>,
}

impl<'de> Deserialize<'de> for AddConnectionsBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: AddConnectionsFields =
            from_object(deserializer, "a JSON object with cluster_id and accounts")?;
        Ok(Self {
            cluster_id: fields.cluster_id,
            accounts: fields.accounts,
        })
    }
}

/// An "Add my connections" request: the library it names (trimmed) and the
/// accounts picked, between 1 and [`MAX_ADD_CONNECTIONS`] of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddConnectionsRequest {
    /// The library (owner cluster id), surrounding whitespace removed.
    pub cluster_id: String,
    /// The accounts picked, in the order sent.
    pub accounts: Vec<AccountRef>,
}

impl AddConnectionsBody {
    /// Checks the account count and returns the request.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when the body names no account or
    /// more than [`MAX_ADD_CONNECTIONS`].
    pub fn validate(self) -> Result<AddConnectionsRequest> {
        if self.accounts.is_empty() || self.accounts.len() > MAX_ADD_CONNECTIONS {
            return Err(LibraryError::Invalid(format!(
                "name between 1 and {MAX_ADD_CONNECTIONS} accounts"
            )));
        }
        Ok(AddConnectionsRequest {
            cluster_id: self.cluster_id.trim().to_string(),
            accounts: self.accounts,
        })
    }
}

/// Parses and validates a `POST /api/config/join/connections` body from its
/// raw bytes, sans IO, the way the route reads it: JSON into
/// [`AddConnectionsBody`], then [`AddConnectionsBody::validate`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape (not an object, an account that is not an object, unknown or
/// duplicate fields, missing fields, wrong types) or that name no account
/// or more than [`MAX_ADD_CONNECTIONS`].
pub fn parse_add_connections_request(body: &[u8]) -> Result<AddConnectionsRequest> {
    serde_json::from_slice::<AddConnectionsBody>(body)
        .map_err(|err| LibraryError::Invalid(format!("connections body: {err}")))?
        .validate()
}

/// `POST /api/config/join/connections/disconnect` body.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `DisconnectFields` enforces
// it.
#[serde(deny_unknown_fields)]
pub struct DisconnectBody {
    /// Provenance-qualified plugin key of the storefront.
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: String,
    /// Store account id.
    #[schema(example = "reader@example.com")]
    account_id: String,
    /// Also disconnect it in every linked library it was shared with;
    /// absent means `false`.
    #[serde(default)]
    #[schema(example = true)]
    everywhere: bool,
}

/// [`DisconnectBody`]'s fields as JSON carries them; read only from a JSON
/// object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DisconnectFields {
    /// See [`DisconnectBody`].
    plugin_key: String,
    /// See [`DisconnectBody`].
    account_id: String,
    /// See [`DisconnectBody`].
    #[serde(default)]
    everywhere: bool,
}

impl<'de> Deserialize<'de> for DisconnectBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: DisconnectFields = from_object(
            deserializer,
            "a JSON object with plugin_key, account_id and optionally everywhere",
        )?;
        Ok(Self {
            plugin_key: fields.plugin_key,
            account_id: fields.account_id,
            everywhere: fields.everywhere,
        })
    }
}

/// A disconnect request: the account and whether to reach every linked
/// library it was shared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisconnectRequest {
    /// The account, as sent.
    pub account: AccountRef,
    /// Also disconnect it in every linked library it was shared with.
    pub everywhere: bool,
}

impl DisconnectBody {
    /// The request the body makes.
    #[must_use]
    pub fn into_request(self) -> DisconnectRequest {
        DisconnectRequest {
            account: AccountRef {
                plugin_key: self.plugin_key,
                account_id: self.account_id,
            },
            everywhere: self.everywhere,
        }
    }
}

/// Parses a `POST /api/config/join/connections/disconnect` body from its
/// raw bytes, sans IO, the way the route reads it: JSON into
/// [`DisconnectBody`], then [`DisconnectBody::into_request`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape: not an object, unknown or duplicate fields, a missing
/// `plugin_key` or `account_id`, or a value of the wrong type (`everywhere`
/// is a boolean when present; `null` is refused).
pub fn parse_disconnect_request(body: &[u8]) -> Result<DisconnectRequest> {
    serde_json::from_slice::<DisconnectBody>(body)
        .map(DisconnectBody::into_request)
        .map_err(|err| LibraryError::Invalid(format!("disconnect body: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_add_body_trims_the_library_and_keeps_the_picks() {
        let request = parse_add_connections_request(
            br#"{"cluster_id":" c1 ","accounts":[{"plugin_key":"p","account_id":"a"}]}"#,
        );
        assert_eq!(
            request.ok(),
            Some(AddConnectionsRequest {
                cluster_id: "c1".into(),
                accounts: vec![AccountRef {
                    plugin_key: "p".into(),
                    account_id: "a".into(),
                }],
            })
        );
    }

    #[test]
    fn an_add_body_names_between_one_and_sixty_four_accounts() {
        let body = |n: usize| {
            let accounts = (0..n)
                .map(|i| format!(r#"{{"plugin_key":"p","account_id":"a{i}"}}"#))
                .collect::<Vec<_>>()
                .join(",");
            format!(r#"{{"cluster_id":"c1","accounts":[{accounts}]}}"#)
        };
        assert!(matches!(
            parse_add_connections_request(body(0).as_bytes()),
            Err(LibraryError::Invalid(_))
        ));
        assert!(parse_add_connections_request(body(MAX_ADD_CONNECTIONS).as_bytes()).is_ok());
        assert!(matches!(
            parse_add_connections_request(body(MAX_ADD_CONNECTIONS + 1).as_bytes()),
            Err(LibraryError::Invalid(_))
        ));
    }

    #[test]
    fn positional_and_repeated_shapes_are_refused() {
        for body in [
            &br#"["c1",[{"plugin_key":"p","account_id":"a"}]]"#[..],
            br#"{"cluster_id":"c1","accounts":[["p","a"]]}"#,
            br#"{"cluster_id":"c1","cluster_id":"c2","accounts":[{"plugin_key":"p","account_id":"a"}]}"#,
            br#"{"cluster_id":"c1","accounts":[{"plugin_key":"p","account_id":"a","x":1}]}"#,
        ] {
            assert!(
                matches!(
                    parse_add_connections_request(body),
                    Err(LibraryError::Invalid(_))
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        for body in [
            &br#"["p","a",true]"#[..],
            br#"{"plugin_key":"p","account_id":"a","everywhere":null}"#,
            br#"{"plugin_key":"p","account_id":"a","account_id":"b"}"#,
        ] {
            assert!(
                matches!(
                    parse_disconnect_request(body),
                    Err(LibraryError::Invalid(_))
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn a_disconnect_body_defaults_to_here_only() {
        let request = parse_disconnect_request(br#"{"plugin_key":"p","account_id":"a"}"#);
        assert_eq!(
            request.ok(),
            Some(DisconnectRequest {
                account: AccountRef {
                    plugin_key: "p".into(),
                    account_id: "a".into(),
                },
                everywhere: false,
            })
        );
    }
}
