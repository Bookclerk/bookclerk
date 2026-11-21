//! Host-served `NOTIFY` binding: a plugin raises the notification types it
//! declared and the host routes them.
//!
//! The plugin names only the type, the labelled values and the optional
//! link, group, alert and patron. The host stamps everything else: the type
//! is qualified as `<plugin id>:<type>`, the title and audience come from
//! the manifest's `[[notifications.types]]` row, the body is the fields one
//! per line, and the source is the plugin id. A patron-audience raise must
//! name a patron who holds the store account the invocation is scoped to,
//! so a plugin can only notify the people whose account it is working for;
//! an invocation without an account (the operator's) cannot raise one.
//! Raises are rate-limited per session ([`RAISES_PER_MINUTE`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bookclerk_library::notifications::{fields_text, qualified_plugin_type, NewNotification};
use bookclerk_library::{LibraryError, LibraryStore, NotifyQueueLimits};
use bookclerk_plugin_abi::notifications::check_raise_params;
use bookclerk_plugin_abi::{
    Invocation, NotificationAudience, NotificationRaiser, NotificationSeverity, NotifyRaiseParams,
    NotifyRaised, PluginError,
};

use crate::PluginManifest;

/// Plugin ABI result (the wire error the guest receives).
type Result<T> = std::result::Result<T, PluginError>;

/// Raises one session may make in a rolling minute.
pub const RAISES_PER_MINUTE: usize = 30;

/// The window [`RAISES_PER_MINUTE`] counts over.
const RAISE_WINDOW: Duration = Duration::from_secs(60);

/// What the manifest declares about one type.
#[derive(Debug, Clone)]
struct DeclaredType {
    /// Who it is for.
    audience: NotificationAudience,
    /// Default severity.
    severity: NotificationSeverity,
    /// Title shown on every notification of the type.
    title: String,
}

/// Raise times of the current window, oldest first.
#[derive(Debug, Default)]
struct RaiseWindow {
    /// When each counted raise happened.
    at: std::collections::VecDeque<Instant>,
}

impl RaiseWindow {
    /// Counts a raise at `now`, or refuses it when the window is full.
    fn admit(&mut self, now: Instant) -> bool {
        while self
            .at
            .front()
            .is_some_and(|t| now.duration_since(*t) >= RAISE_WINDOW)
        {
            self.at.pop_front();
        }
        if self.at.len() >= RAISES_PER_MINUTE {
            return false;
        }
        self.at.push_back(now);
        true
    }
}

/// Host-owned facts a session needs to hand a guest a `NOTIFY` binding.
#[derive(Clone)]
pub struct NotifyOutbox {
    /// Library the notifications are routed in.
    store: LibraryStore,
    /// Manifest plugin id: qualifies the types and is the source.
    plugin_id: String,
    /// Canonical plugin key the store accounts are keyed by.
    plugin_key: String,
    /// Declared types by unqualified id.
    types: Arc<BTreeMap<String, DeclaredType>>,
    /// Queue limits the send jobs are admitted under.
    limits: NotifyQueueLimits,
    /// Raise times shared by every open of the session.
    window: Arc<Mutex<RaiseWindow>>,
}

impl NotifyOutbox {
    /// Builds the hook, or `None` when the manifest declares no types (the
    /// guest then sees no `NOTIFY` binding).
    ///
    /// # Arguments
    ///
    /// * `store` - The library notifications are routed in.
    /// * `plugin_key` - Canonical plugin key (the store accounts' key).
    /// * `manifest` - The plugin's manifest.
    /// * `limits` - The `[jobs]` queue limits.
    #[must_use]
    pub fn new(
        store: LibraryStore,
        plugin_key: &str,
        manifest: &PluginManifest,
        limits: NotifyQueueLimits,
    ) -> Option<Self> {
        let types: BTreeMap<String, DeclaredType> = manifest
            .notifications
            .types
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    DeclaredType {
                        audience: t.audience,
                        severity: t.severity,
                        title: t.title.clone(),
                    },
                )
            })
            .collect();
        if types.is_empty() {
            return None;
        }
        Some(Self {
            store,
            plugin_id: manifest.id.clone(),
            plugin_key: plugin_key.to_string(),
            types: Arc::new(types),
            limits,
            window: Arc::new(Mutex::new(RaiseWindow::default())),
        })
    }

    /// Raiser bound to one `PluginWorker.open` invocation (its account
    /// scope decides which patrons it may notify).
    #[must_use]
    pub fn raiser(&self, invocation: &Invocation) -> OutboxNotificationRaiser {
        OutboxNotificationRaiser {
            outbox: self.clone(),
            account_id: invocation.account_id.clone(),
        }
    }
}

/// [`NotificationRaiser`] server for one invocation.
pub struct OutboxNotificationRaiser {
    /// The session's hook.
    outbox: NotifyOutbox,
    /// `Invocation.accountId`; empty for an operator invocation.
    account_id: String,
}

impl OutboxNotificationRaiser {
    /// Checks `params` and builds the notification the router accepts.
    ///
    /// # Errors
    ///
    /// * `invalid_params` for a raise the message rules refuse, a type the
    ///   manifest does not declare, or a patron that does not match the
    ///   type's audience.
    /// * `forbidden` for a patron outside the invocation's account scope.
    async fn notification(&self, params: NotifyRaiseParams) -> Result<NewNotification> {
        check_raise_params(&params).map_err(|err| PluginError::invalid_params(err.to_string()))?;
        let declared = self
            .outbox
            .types
            .get(&params.notification_type)
            .ok_or_else(|| {
                PluginError::invalid_params(format!(
                    "notification type `{}` is not declared in [[notifications.types]]",
                    params.notification_type
                ))
            })?;
        let patron_text = params.patron_id.as_deref().filter(|p| !p.is_empty());
        let patron = match (declared.audience, patron_text) {
            (NotificationAudience::Patron, Some(text)) => Some(self.patron_in_scope(text).await?),
            (NotificationAudience::Patron, None) => {
                return Err(PluginError::invalid_params(
                    "a patron notification names its patron",
                ));
            }
            (NotificationAudience::Operational | NotificationAudience::Account, None) => None,
            (NotificationAudience::Operational | NotificationAudience::Account, Some(_)) => {
                return Err(PluginError::invalid_params(
                    "only a patron notification names a patron",
                ));
            }
        };
        let text = fields_text(&params.fields);
        Ok(NewNotification {
            notification_type: qualified_plugin_type(
                &self.outbox.plugin_id,
                &params.notification_type,
            ),
            audience: declared.audience,
            severity: declared.severity,
            title: declared.title.clone(),
            text: if text.is_empty() {
                declared.title.clone()
            } else {
                text
            },
            link_path: params.link_path.filter(|p| !p.is_empty()),
            fields: params.fields,
            group_key: params.group_key.filter(|g| !g.is_empty()),
            alert_action: params.alert_action,
            alert_key: params.alert_key.filter(|k| !k.is_empty()),
            source: self.outbox.plugin_id.clone(),
            patron,
        })
    }

    /// The patron `text` names, when they hold the invocation's account.
    ///
    /// # Errors
    ///
    /// `forbidden` when the invocation has no account, the id is not a
    /// patron id, or the patron does not hold the account (the three answer
    /// alike so a plugin cannot probe for patrons); `unavailable` when the
    /// library cannot be read.
    async fn patron_in_scope(&self, text: &str) -> Result<i64> {
        let outside = || PluginError::forbidden("the patron is outside this invocation's accounts");
        let patron = bookclerk_plugin_abi::notifications::scoped_patron_id(&self.account_id, text)
            .ok_or_else(outside)?;
        let holds = self
            .outbox
            .store
            .user_holds_account(patron, &self.outbox.plugin_key, &self.account_id)
            .await
            .map_err(|err| PluginError::unavailable(format!("library: {err}")))?;
        if holds {
            Ok(patron)
        } else {
            Err(outside())
        }
    }

    /// Counts a raise against the session's window.
    ///
    /// # Errors
    ///
    /// `rate_limited` when the window is full.
    fn admit(&self) -> Result<()> {
        let admitted = self
            .outbox
            .window
            .lock()
            .map(|mut window| window.admit(Instant::now()))
            .unwrap_or(false);
        if admitted {
            Ok(())
        } else {
            Err(PluginError::rate_limited(
                format!("at most {RAISES_PER_MINUTE} notifications a minute"),
                None,
            ))
        }
    }
}

#[async_trait(?Send)]
impl NotificationRaiser for OutboxNotificationRaiser {
    async fn raise(&self, params: NotifyRaiseParams) -> Result<NotifyRaised> {
        // Count the raise first, so refused raises spend the budget too and
        // cannot drive unbounded scope lookups.
        self.admit()?;
        let notification = self.notification(params).await?;
        let routed = self
            .outbox
            .store
            .route_notification(&notification, self.outbox.limits)
            .await
            .map_err(|err| match err {
                LibraryError::Invalid(message) => PluginError::invalid_params(message),
                // The patron was checked; a vanished one answers like one
                // outside the scope.
                LibraryError::NotFound(_) => {
                    PluginError::forbidden("the patron is outside this invocation's accounts")
                }
                other => PluginError::unavailable(format!("library: {other}")),
            })?;
        Ok(NotifyRaised {
            notification_id: routed.notification_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::{NotificationField, PluginErrorCode};

    /// An empty in-memory library.
    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        )
    }

    /// A plugin declaring one operational and one patron type.
    fn manifest(types: &str) -> PluginManifest {
        PluginManifest::parse(&format!(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
{types}
"#
        ))
        .expect("manifest")
    }

    /// The two types the tests raise.
    const TYPES: &str = r#"
[[notifications.types]]
id = "sync_failed"
audience = "operational"
severity = "warning"
title = "Sync failed"

[[notifications.types]]
id = "new_episode"
audience = "patron"
title = "New episode"
"#;

    /// Queue limits with room.
    const LIMITS: NotifyQueueLimits = NotifyQueueLimits {
        max_pending: 1_000,
        max_attempts: 3,
    };

    /// An invocation scoped to `account`.
    fn invocation(account: &str) -> Invocation {
        Invocation {
            id: "inv-1".into(),
            account_id: account.into(),
            ..Invocation::default()
        }
    }

    /// A raise of `notification_type` with one field.
    fn raise(notification_type: &str, patron: Option<&str>) -> NotifyRaiseParams {
        NotifyRaiseParams {
            notification_type: notification_type.into(),
            fields: vec![NotificationField {
                name: "Feed".into(),
                value: "Daily".into(),
            }],
            patron_id: patron.map(str::to_string),
            ..NotifyRaiseParams::default()
        }
    }

    #[tokio::test]
    async fn no_declared_type_means_no_binding() {
        let store = memory_store().await;
        assert!(NotifyOutbox::new(store, "demo", &manifest(""), LIMITS).is_none());
    }

    #[tokio::test]
    async fn a_declared_type_is_stamped_by_the_host_and_stored() {
        let store = memory_store().await;
        let outbox = NotifyOutbox::new(store.clone(), "demo", &manifest(TYPES), LIMITS)
            .expect("declared types");
        let raiser = outbox.raiser(&invocation(""));
        let raised = raiser
            .raise(raise("sync_failed", None))
            .await
            .expect("raised");
        let page = store.notify_deliveries(None, None, 10).await.unwrap();
        // Nothing routes the type yet, so it is stored without deliveries.
        assert!(page.items.is_empty());
        let built = raiser
            .notification(raise("sync_failed", None))
            .await
            .unwrap();
        assert_eq!(built.notification_type, "demo:sync_failed");
        assert_eq!(built.title, "Sync failed");
        assert_eq!(built.text, "Feed: Daily");
        assert_eq!(built.source, "demo");
        assert_eq!(built.severity, NotificationSeverity::Warning);
        assert!(!raised.notification_id.is_empty());
    }

    #[tokio::test]
    async fn undeclared_types_and_misplaced_patrons_are_refused() {
        let store = memory_store().await;
        let outbox = NotifyOutbox::new(store, "demo", &manifest(TYPES), LIMITS).unwrap();
        let raiser = outbox.raiser(&invocation("acct-1"));
        let err = raiser.raise(raise("other", None)).await.unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        let err = raiser
            .raise(raise("sync_failed", Some("1")))
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        let err = raiser.raise(raise("new_episode", None)).await.unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn a_patron_outside_the_invocations_account_is_forbidden() {
        let store = memory_store().await;
        let outbox = NotifyOutbox::new(store, "demo", &manifest(TYPES), LIMITS).unwrap();
        // The operator's invocation has no account, so no patron is in scope.
        let err = outbox
            .raiser(&invocation(""))
            .raise(raise("new_episode", Some("1")))
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::Forbidden);
        for patron in ["1", "not-a-number"] {
            let err = outbox
                .raiser(&invocation("acct-1"))
                .raise(raise("new_episode", Some(patron)))
                .await
                .unwrap_err();
            assert_eq!(err.code, PluginErrorCode::Forbidden, "{patron}");
        }
    }

    #[tokio::test]
    async fn raises_past_the_window_are_rate_limited() {
        let store = memory_store().await;
        let outbox = NotifyOutbox::new(store, "demo", &manifest(TYPES), LIMITS).unwrap();
        for _ in 0..RAISES_PER_MINUTE {
            outbox
                .raiser(&invocation(""))
                .raise(raise("sync_failed", None))
                .await
                .unwrap();
        }
        // Every open of the session shares one window.
        let err = outbox
            .raiser(&invocation(""))
            .raise(raise("sync_failed", None))
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::RateLimited);
    }

    #[test]
    fn the_window_admits_a_minutes_worth_then_refuses_until_it_slides() {
        let mut window = RaiseWindow::default();
        let start = Instant::now();
        for _ in 0..RAISES_PER_MINUTE {
            assert!(window.admit(start));
        }
        assert!(!window.admit(start + Duration::from_secs(59)));
        assert!(window.admit(start + RAISE_WINDOW));
    }
}
