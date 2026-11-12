//! Store connections across the libraries this install is a branch of
//! ("Accounts on every instance" in the library catalog network ADR).
//!
//! - `POST /api/config/join/connections` `{cluster_id, accounts}`: "Add my
//!   connections to *library*". The branch's person picks some of their own
//!   connected accounts; the branch seals them to the library's owner box
//!   key over the link's socket ([`HostClientHandle::offer_accounts`](bookclerk_node_client::HostClientHandle::offer_accounts)),
//!   records where each went (`account_shares`, peer `link:<cluster>`) and
//!   answers what the library kept.
//! - `POST /api/config/join/connections/disconnect` `{plugin_key,
//!   account_id, everywhere}`: disconnects one of the person's accounts
//!   here, and with `everywhere` also in every linked library it was
//!   shared with (`accounts.remove` up each link).
//!
//! Only the branch's person (the install's Owner the joins belong to,
//! [`branch_person`]) may call them: the library attributes what arrives
//! to the user its node row names as the branch's owner. Only accounts of
//! storefronts every installed copy declares `credentials = "shareable"`
//! travel.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_library::control_plane::{
    account_share_peers, accounts_to_carry, branch_person, link_peer, list_cluster_links,
    load_cluster_link, record_accounts_sent_to, withdraw_person_account, Withdrawn,
};
use bookclerk_node_client::OwnerCallError;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::join::join_error;
use crate::api::AppState;
use crate::auth::UserCaller;

/// Most accounts one "Add my connections" request names.
const MAX_ADD_ACCOUNTS: usize = bookclerk_node_proto::MAX_BUNDLE_ACCOUNTS;

/// One store account, by storefront plugin key and store account id.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountRef {
    /// Provenance-qualified plugin key of the storefront.
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: String,
    /// Store account id.
    #[schema(example = "reader@example.com")]
    account_id: String,
}

/// `POST /api/config/join/connections` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddConnectionsBody {
    /// The library (owner cluster id) to add the connections to.
    #[schema(example = "6d1f2e3a-4b5c-4d6e-8f7a-9b0c1d2e3f4a")]
    cluster_id: String,
    /// The person's accounts to add; at most 64.
    accounts: Vec<AccountRef>,
}

/// JSON body after `POST /api/config/join/connections`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ConnectionsAdded {
    /// Accounts now connected in the library for the person.
    #[schema(example = 1)]
    pub added: u64,
    /// Accounts the person had already connected there.
    #[schema(example = 0)]
    pub already_here: u64,
    /// Accounts connected to someone else there; left as they are.
    #[schema(example = 0)]
    pub taken: u64,
    /// Accounts that could not be kept there.
    #[schema(example = 0)]
    pub failed: u64,
    /// Accounts not sent: not the person's, not connected, or of a
    /// storefront whose sign-ins can't be shared.
    #[schema(example = 0)]
    pub not_sent: u64,
    /// The library's operator does not let the person connect store
    /// accounts there; nothing was kept.
    #[schema(example = false)]
    pub not_allowed: bool,
}

/// `POST /api/config/join/connections/disconnect` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DisconnectBody {
    /// Provenance-qualified plugin key of the storefront.
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: String,
    /// Store account id.
    #[schema(example = "reader@example.com")]
    account_id: String,
    /// Also disconnect it in every linked library it was shared with.
    #[serde(default)]
    #[schema(example = true)]
    everywhere: bool,
}

/// JSON body after `POST /api/config/join/connections/disconnect`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Disconnected {
    /// The account was the person's here and is disconnected here now.
    #[schema(example = true)]
    pub here: bool,
    /// Linked libraries (owner cluster ids) that confirmed the disconnect.
    pub libraries: Vec<String>,
    /// Linked libraries that could not be reached or refused; the account
    /// stays connected there until the person disconnects it there.
    pub unreached: Vec<String>,
}

/// The branch's person, when `caller` is that user.
async fn caller_person(state: &Arc<AppState>, caller: UserCaller) -> Result<i64, Response> {
    let UserCaller::User { user_id } = caller else {
        return Err(join_error(
            StatusCode::FORBIDDEN,
            "not_branch_person",
            "sign in as the person this branch belongs to",
        ));
    };
    let store = state.library_snapshot().await;
    match branch_person(&store, Some(user_id)).await {
        Ok(Some(person)) if person == user_id => Ok(user_id),
        Ok(_) => Err(join_error(
            StatusCode::FORBIDDEN,
            "not_branch_person",
            "sign in as the person this branch belongs to",
        )),
        Err(err) => Err(crate::config_authority::events_error(err)),
    }
}

/// `POST /api/config/join/connections`: "Add my connections to *library*".
#[utoipa::path(
    post,
    path = "/api/config/join/connections",
    request_body = AddConnectionsBody,
    responses(
        (status = 200, description = "What the library kept.", body = ConnectionsAdded),
        (status = 400, description = "No accounts, or more than 64 (`invalid_request`).", body = crate::http_error::ErrorDetail, example = json!({"error": "invalid_request", "detail": "name between 1 and 64 accounts"})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not the person this branch belongs to (`not_branch_person`).", body = crate::http_error::ErrorDetail, example = json!({"error": "not_branch_person", "detail": "sign in as the person this branch belongs to"})),
        (status = 404, description = "This install is not a branch of that library (`not_found`).", body = crate::http_error::ErrorDetail, example = json!({"error": "not_found", "detail": "this install is not a branch of that library"})),
        (status = 409, description = "The library refused the offer (`refused`).", body = crate::http_error::ErrorDetail, example = json!({"error": "refused", "detail": "no_owner"})),
        (status = 503, description = "The library could not be reached (`unavailable`).", body = crate::http_error::ErrorDetail, example = json!({"error": "unavailable", "detail": "the library could not be reached; try again"})),
        (status = 500, description = "The library database could not be read or written.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_join_connections(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<AddConnectionsBody>,
) -> Response {
    if body.accounts.is_empty() || body.accounts.len() > MAX_ADD_ACCOUNTS {
        return join_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "name between 1 and 64 accounts",
        );
    }
    let person = match caller_person(&state, caller).await {
        Ok(person) => person,
        Err(refused) => return refused,
    };
    let store = state.library_snapshot().await;
    let cluster_id = body.cluster_id.trim();
    let link = match load_cluster_link(&store, cluster_id).await {
        Ok(Some(link)) if link.is_active() => link,
        Ok(_) => {
            return join_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "this install is not a branch of that library",
            )
        }
        Err(err) => return crate::config_authority::events_error(err),
    };
    let shareable = crate::node_api::branch::shareable_plugin_keys(&state, &store).await;
    let is_shareable = move |key: &str| shareable.contains(key);
    let mine = match accounts_to_carry(&store, person, &is_shareable).await {
        Ok(mine) => mine,
        Err(err) => return crate::config_authority::events_error(err),
    };
    let wanted: Vec<_> = mine
        .into_iter()
        .filter(|account| {
            body.accounts.iter().any(|picked| {
                picked.plugin_key == account.plugin_key && picked.account_id == account.account_id
            })
        })
        .collect();
    let not_sent = count(body.accounts.len().saturating_sub(wanted.len()));
    if wanted.is_empty() {
        return Json(ConnectionsAdded {
            added: 0,
            already_here: 0,
            taken: 0,
            failed: 0,
            not_sent,
            not_allowed: false,
        })
        .into_response();
    }
    let Some(handle) = super::link_handle(&link.owner_cluster_id, &link.remote_node_id) else {
        return unavailable();
    };
    // Recorded before the offer leaves: nothing is sent without a record
    // of where it went.
    if let Err(err) = record_accounts_sent_to(&store, &link_peer(cluster_id), &wanted).await {
        return crate::config_authority::events_error(err);
    }
    let sent = wanted.len();
    match handle.offer_accounts(wanted).await {
        Ok(report) => {
            let detail = json!({
                "cluster_id": cluster_id,
                "sent": sent,
                "added": report.added,
                "not_allowed": report.not_allowed,
            })
            .to_string();
            if let Err(err) = store
                .insert_security_audit_event(
                    &format!("user:{person}"),
                    "connections_offered",
                    Some(&detail),
                )
                .await
            {
                tracing::warn!(%cluster_id, error = %err, "could not audit offered store connections");
            }
            Json(ConnectionsAdded {
                added: report.added,
                already_here: report.already_here,
                taken: report.taken,
                failed: report.failed,
                not_sent,
                not_allowed: report.not_allowed,
            })
            .into_response()
        }
        Err(OwnerCallError::Refused(reason)) => {
            join_error(StatusCode::CONFLICT, "refused", &reason)
        }
        Err(OwnerCallError::Lost | OwnerCallError::Unavailable) => unavailable(),
    }
}

/// `POST /api/config/join/connections/disconnect`: disconnects one of the
/// person's store accounts here, and with `everywhere` in every linked
/// library it was shared with.
#[utoipa::path(
    post,
    path = "/api/config/join/connections/disconnect",
    request_body = DisconnectBody,
    responses(
        (status = 200, description = "Where the account is disconnected now.", body = Disconnected),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller is not the person this branch belongs to (`not_branch_person`).", body = crate::http_error::ErrorDetail, example = json!({"error": "not_branch_person", "detail": "sign in as the person this branch belongs to"})),
        (status = 500, description = "The library database could not be read or written.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_join_connections_disconnect(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<DisconnectBody>,
) -> Response {
    let person = match caller_person(&state, caller).await {
        Ok(person) => person,
        Err(refused) => return refused,
    };
    let store = state.library_snapshot().await;
    let mut libraries = Vec::new();
    let mut unreached = Vec::new();
    if body.everywhere {
        let peers = match account_share_peers(&store, &body.plugin_key, &body.account_id).await {
            Ok(peers) => peers,
            Err(err) => return crate::config_authority::events_error(err),
        };
        let links = match list_cluster_links(&store).await {
            Ok(links) => links,
            Err(err) => return crate::config_authority::events_error(err),
        };
        for link in links.iter().filter(|link| link.is_active()) {
            if !peers.contains(&link_peer(&link.owner_cluster_id)) {
                continue;
            }
            let reached = match super::link_handle(&link.owner_cluster_id, &link.remote_node_id) {
                Some(handle) => handle
                    .withdraw_account(&body.plugin_key, &body.account_id)
                    .await
                    .is_ok(),
                None => false,
            };
            if reached {
                libraries.push(link.owner_cluster_id.clone());
            } else {
                unreached.push(link.owner_cluster_id.clone());
            }
        }
    }
    let here =
        match withdraw_person_account(&store, person, &body.plugin_key, &body.account_id).await {
            Ok(Withdrawn::Unlinked) => true,
            Ok(Withdrawn::NotTheirs) => false,
            Err(err) => return crate::config_authority::events_error(err),
        };
    let detail = json!({
        "plugin_key": body.plugin_key,
        "everywhere": body.everywhere,
        "here": here,
        "libraries": libraries,
        "unreached": unreached,
    })
    .to_string();
    if let Err(err) = store
        .insert_security_audit_event(
            &format!("user:{person}"),
            "connection_disconnected",
            Some(&detail),
        )
        .await
    {
        tracing::warn!(error = %err, "could not audit a disconnected store connection");
    }
    Json(Disconnected {
        here,
        libraries,
        unreached,
    })
    .into_response()
}

/// `503 unavailable`: the library could not be reached.
fn unavailable() -> Response {
    join_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "the library could not be reached; try again",
    )
}

/// A count as the answer's `u64`.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
