//! A branch keeping its person's single-keeper store accounts fresh in the
//! libraries it shares them with ("one refresh keeper" in the library
//! catalog network ADR; [`bookclerk_library::control_plane::apply_keeper_update`]
//! is the library's side).
//!
//! Each link runs one [`SignInPusher`]. A pass reads the accounts this
//! install shares with that library (`account_shares`, peer
//! `link:<cluster>`), keeps those whose storefront is not shareable, and
//! for each whose sign-in here differs from the one last pushed there
//! sends it up the link (`accounts.update`) with a revision above the last
//! one sent ([`bookclerk_library::control_plane::KeptShare::next_revision`]). The library applies it
//! only when it is newer than the last one it applied, so a push that is
//! retried, or overtaken by a newer one, changes nothing there. A push the
//! library does not take from this branch yet (`not_shared`, such as one
//! that overtook its offer) is sent again on the next pass.
//!
//! A sign-in is told apart by the time of the account's newest secret
//! write here: a sign-in in Accounts, a refresh a storefront handed back,
//! or a sign-in that arrived from the library at join all change it. Any
//! change counts, an earlier time included, so a clock that went back
//! can't hide a sign-in.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    accounts_to_carry_where, kept_shares, load_cluster_link, record_pushed_revision,
    sign_in_revision,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::{HostClientHandle, OwnerCallError};
use bookclerk_node_proto::KeeperOutcome;
use tokio::sync::watch;

use crate::api::AppState;

/// How often a link's pusher looks for newer sign-ins while connected.
pub const PUSH_POLL: Duration = Duration::from_secs(60);

/// Longest one push may wait for the library's answer.
const PUSH_TIMEOUT: Duration = Duration::from_secs(30);

/// Pushes the person's newer single-keeper sign-ins up one link.
pub struct SignInPusher {
    /// Daemon state, for the plugins' credential declarations.
    pub state: Arc<AppState>,
    /// This install's library.
    pub library: LibraryStore,
    /// The library this link joined.
    pub cluster_id: String,
    /// The link's client.
    pub handle: HostClientHandle,
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushPass {
    /// Sign-ins the library applied.
    pub applied: usize,
    /// Sign-ins the library already had; recorded so they are not sent
    /// again.
    pub settled: usize,
    /// Sign-ins the library does not take from this branch (it doesn't
    /// count this branch as the account's keeper there, at least not yet);
    /// sent again next pass, and not logged.
    pub not_kept: usize,
    /// Sign-ins that could not be sent this time; tried again next pass.
    pub retry: usize,
}

impl SignInPusher {
    /// Runs passes until `shutdown` turns true: at once, on each new
    /// connection, and every [`PUSH_POLL`].
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut connections = self.handle.connections();
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.handle.is_connected() {
                match self.push_once().await {
                    Ok(pass) if pass.applied + pass.settled + pass.retry == 0 => {}
                    Ok(pass) => tracing::info!(
                        cluster_id = %self.cluster_id,
                        applied = pass.applied,
                        settled = pass.settled,
                        retry = pass.retry,
                        "pushed newer store sign-ins to the library"
                    ),
                    Err(err) => tracing::warn!(
                        cluster_id = %self.cluster_id,
                        error = %err,
                        "could not push store sign-ins to the library; retrying later"
                    ),
                }
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                changed = connections.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(PUSH_POLL) => {}
            }
        }
    }

    /// One pass over this link's kept accounts.
    ///
    /// # Errors
    ///
    /// Returns a read error from before any push.
    pub async fn push_once(&self) -> anyhow::Result<PushPass> {
        let mut pass = PushPass::default();
        let shares: Vec<_> = kept_shares(&self.library)
            .await?
            .into_iter()
            .filter(|share| share.cluster_id == self.cluster_id)
            .collect();
        if shares.is_empty() {
            return Ok(pass);
        }
        // Only the person this link joined for has accounts kept through it.
        let Some(person) = load_cluster_link(&self.library, &self.cluster_id)
            .await?
            .filter(|link| link.is_active())
            .and_then(|link| link.person_user_id)
        else {
            return Ok(pass);
        };
        let shareable =
            crate::node_api::branch::shareable_plugin_keys(&self.state, &self.library).await;
        for share in shares {
            if shareable.contains(&share.plugin_key) {
                continue;
            }
            let Some(sign_in) =
                sign_in_revision(&self.library, &share.plugin_key, &share.account_id).await?
            else {
                continue;
            };
            let Some(revision) = share.next_revision(sign_in) else {
                continue;
            };
            let (plugin_key, account_id) = (share.plugin_key.as_str(), share.account_id.as_str());
            let keep = |key: &str, id: &str| key == plugin_key && id == account_id;
            let Some(account) = accounts_to_carry_where(&self.library, person, &keep)
                .await?
                .into_iter()
                .next()
            else {
                // No longer the person's, or no secret left: nothing to keep
                // fresh.
                continue;
            };
            let pushed =
                tokio::time::timeout(PUSH_TIMEOUT, self.handle.push_sign_in(account, revision))
                    .await
                    .unwrap_or(Err(OwnerCallError::Unavailable));
            match pushed {
                Ok(outcome @ (KeeperOutcome::Applied | KeeperOutcome::Stale)) => {
                    record_pushed_revision(
                        &self.library,
                        plugin_key,
                        account_id,
                        &self.cluster_id,
                        revision,
                        sign_in,
                    )
                    .await?;
                    if outcome == KeeperOutcome::Applied {
                        pass.applied += 1;
                    } else {
                        pass.settled += 1;
                    }
                }
                // Not (yet) this branch's to keep there: an offer still on its
                // way, or one retried later, may make it so. Not recorded,
                // so the next pass sends it again.
                Ok(KeeperOutcome::NotShared) => pass.not_kept += 1,
                Err(OwnerCallError::Refused(reason)) => {
                    tracing::warn!(
                        cluster_id = %self.cluster_id,
                        %plugin_key,
                        %reason,
                        "the library refused a pushed sign-in"
                    );
                    pass.retry += 1;
                }
                Err(OwnerCallError::Lost | OwnerCallError::Unavailable) => pass.retry += 1,
            }
        }
        Ok(pass)
    }
}
