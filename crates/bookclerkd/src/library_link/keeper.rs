//! A branch keeping its person's single-keeper store accounts fresh in the
//! libraries it shares them with ("one refresh keeper" in the library
//! catalog network ADR; [`bookclerk_library::control_plane::apply_keeper_update`]
//! is the library's side).
//!
//! Each link runs one [`SignInPusher`]. A pass reads the accounts this
//! install shares with that library (`account_shares`, peer
//! `link:<library>`), keeps those whose storefront is not shareable, and
//! for each whose sign-in here differs from the one last pushed there
//! sends it up the link (`accounts.update`) with a revision above the last
//! one sent ([`bookclerk_library::control_plane::KeptShare::next_revision`]). The library applies it
//! only when it is newer than the last one it applied, so a push that is
//! retried, or overtaken by a newer one, changes nothing there. A push the
//! library does not take from this branch yet (`not_shared`, such as one
//! that overtook its offer) is sent again on the next pass.
//!
//! A sign-in is told apart by a fingerprint of the account's secrets here
//! ([`bookclerk_library::control_plane::SignIn`]): a sign-in in Accounts, a
//! refresh a storefront handed back, or a sign-in that arrived from the
//! library at join all change it, even two writes in one millisecond or
//! one whose clock went back. A push the library answers `stale` (it
//! already holds that revision, as after a lost answer) uses the revision
//! up but leaves the sign-in unsent, so the next pass sends it above it.
//!
//! The first push of an account can meet a refresh the library saved after
//! it sent the account here: the library then answers `reconcile` with its
//! own sign-in instead of applying the push
//! ([`bookclerk_library::control_plane::keeper_reconcile`]). When this
//! install's copy is still the one the library sent, it takes the
//! library's sign-in and pushes that back, which the library applies;
//! otherwise its own sign-in is newer, and it pushes it again with
//! `keeps_own`.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    accounts_to_carry_where, adopt_library_sign_in, kept_shares, load_library_link,
    record_pushed_revision, sign_in_revision, still_as_received, KeptShare,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::{HostClientHandle, OwnerCallError, SignInPushed};
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
    pub library_id: String,
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
    /// First pushes the library answered with its own newer sign-in, which
    /// this install took in place of its unchanged copy.
    pub taken: usize,
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
                    Ok(pass) if pass.applied + pass.settled + pass.retry + pass.taken == 0 => {}
                    Ok(pass) => tracing::info!(
                        library_id = %self.library_id,
                        applied = pass.applied,
                        settled = pass.settled,
                        retry = pass.retry,
                        taken = pass.taken,
                        "pushed newer store sign-ins to the library"
                    ),
                    Err(err) => tracing::warn!(
                        library_id = %self.library_id,
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
            .filter(|share| share.library_id == self.library_id)
            .collect();
        if shares.is_empty() {
            return Ok(pass);
        }
        // Only the person this link joined for has accounts kept through it.
        let Some(person) = load_library_link(&self.library, &self.library_id)
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
            self.push_share(&share, person, &mut pass).await?;
        }
        Ok(pass)
    }

    /// Pushes one kept account's sign-in when it differs from the one last
    /// pushed, taking the library's newer sign-in or insisting on this
    /// one's when the library answers `reconcile`.
    ///
    /// # Errors
    ///
    /// Returns a read or write error here.
    async fn push_share(
        &self,
        share: &KeptShare,
        person: i64,
        pass: &mut PushPass,
    ) -> anyhow::Result<()> {
        let (plugin_key, account_id) = (share.plugin_key.as_str(), share.account_id.as_str());
        let Some(sign_in) = sign_in_revision(&self.library, plugin_key, account_id).await? else {
            return Ok(());
        };
        let Some(revision) = share.next_revision(&sign_in) else {
            return Ok(());
        };
        let keep = |key: &str, id: &str| key == plugin_key && id == account_id;
        let Some(account) = accounts_to_carry_where(&self.library, person, &keep)
            .await?
            .into_iter()
            .next()
        else {
            // No longer the person's, or no secret left: nothing to keep
            // fresh.
            return Ok(());
        };
        // Whether this install's copy is still the one the library sent,
        // read before the push so a sign-in made here meanwhile counts.
        let as_received =
            still_as_received(&self.library, plugin_key, account_id, &self.library_id).await?;
        match self.push(account.clone(), revision, false).await {
            Ok(SignInPushed {
                outcome: KeeperOutcome::Reconcile,
                library_sign_in: Some(theirs),
            }) => {
                if as_received
                    && adopt_library_sign_in(&self.library, &self.library_id, &theirs).await?
                {
                    pass.taken += 1;
                    // Pushed back, it matches the library's and is applied,
                    // so the library stops keeping the account itself.
                    let Some(taken) =
                        sign_in_revision(&self.library, plugin_key, account_id).await?
                    else {
                        return Ok(());
                    };
                    let Some(next) = share.next_revision(&taken) else {
                        return Ok(());
                    };
                    let answer = self.push(theirs, next, false).await;
                    self.settle(share, answer, next, taken.fingerprint, pass)
                        .await
                } else {
                    let answer = self.push(account, revision, true).await;
                    self.settle(share, answer, revision, sign_in.fingerprint, pass)
                        .await
                }
            }
            answer => {
                self.settle(share, answer, revision, sign_in.fingerprint, pass)
                    .await
            }
        }
    }

    /// One push, bounded by [`PUSH_TIMEOUT`].
    async fn push(
        &self,
        account: bookclerk_node_proto::CarriedAccount,
        revision: i64,
        keeps_own: bool,
    ) -> Result<SignInPushed, OwnerCallError> {
        tokio::time::timeout(
            PUSH_TIMEOUT,
            self.handle.push_sign_in(account, revision, keeps_own),
        )
        .await
        .unwrap_or(Err(OwnerCallError::Unavailable))
    }

    /// Records what the library did with a push of `revision` of the
    /// sign-in whose fingerprint is `fingerprint`, and counts it.
    ///
    /// # Errors
    ///
    /// Returns the write error.
    async fn settle(
        &self,
        share: &KeptShare,
        answer: Result<SignInPushed, OwnerCallError>,
        revision: i64,
        fingerprint: i64,
        pass: &mut PushPass,
    ) -> anyhow::Result<()> {
        let (plugin_key, account_id) = (share.plugin_key.as_str(), share.account_id.as_str());
        match answer.map(|answer| answer.outcome) {
            Ok(KeeperOutcome::Applied) => {
                record_pushed_revision(
                    &self.library,
                    plugin_key,
                    account_id,
                    &self.library_id,
                    revision,
                    fingerprint,
                )
                .await?;
                pass.applied += 1;
            }
            // The library already holds this revision or a newer one: an
            // earlier push it applied whose answer was lost, so this
            // sign-in may not be what it holds. The revision is used up,
            // but the sign-in stays unsent: the next pass sends it again
            // above it.
            Ok(KeeperOutcome::Stale) => {
                record_pushed_revision(
                    &self.library,
                    plugin_key,
                    account_id,
                    &self.library_id,
                    revision,
                    share.observed,
                )
                .await?;
                pass.settled += 1;
            }
            // Not (yet) this branch's to keep there: an offer still on its
            // way, or one retried later, may make it so. Not recorded,
            // so the next pass sends it again.
            Ok(KeeperOutcome::NotShared) => pass.not_kept += 1,
            // A second `reconcile` (the library refreshed again while this
            // push was on its way): not recorded, so the next pass decides
            // again.
            Ok(KeeperOutcome::Reconcile) => pass.retry += 1,
            Err(OwnerCallError::Refused(reason)) => {
                tracing::warn!(
                    library_id = %self.library_id,
                    %plugin_key,
                    %reason,
                    "the library refused a pushed sign-in"
                );
                pass.retry += 1;
            }
            Err(OwnerCallError::Lost | OwnerCallError::Unavailable) => pass.retry += 1,
        }
        Ok(())
    }
}
