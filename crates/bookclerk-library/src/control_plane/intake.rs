//! `core.intake`: whether a storage plugin instance is a drop folder, and
//! its settings (`docs/server/drop-folders.md`).
//!
//! A storage instance is an acquire destination unless its `core.intake`
//! document holds intake settings; then it is a drop folder, which the host
//! lists and ingests from and never writes acquired books to. The document
//! is host data on the instance's scope, beside the plugin's own
//! `plugin.config` and the destination's `core.destination` documents.
//!
//! The role is read when the instance's guest starts, so it changes only
//! while the instance is deployed nowhere: [`set_intake`] refuses to turn a
//! deployed destination into a drop folder or back. The settings of a drop
//! folder that is already one may change at any time; the host reads them
//! on every pass.

use bookclerk_config::{parse_intake_settings, IntakeSettings};
use serde::{Deserialize, Serialize};

use super::deployments::{list_deployments_for_instance, DesiredState};
use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument, PLUGIN_INSTANCE_SCOPE_TYPE,
};
use super::instances::PluginInstanceId;
use super::{document, ConfigActor, ConfigurationDocument};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Namespace of a storage instance's role document.
pub const INTAKE_NAMESPACE: &str = "core.intake";

/// Schema version of [`IntakeRoleV1`].
pub const INTAKE_SCHEMA_VERSION: i64 = 1;

/// How many times [`set_intake`] retries a lost compare-and-swap before it
/// reports a conflict.
const SET_ATTEMPTS: u32 = 4;

/// The body of a `core.intake` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IntakeRoleV1 {
    /// The drop folder's settings; `None` keeps the instance a destination.
    pub intake: Option<IntakeSettings>,
}

/// The `core.intake` address for one plugin instance.
#[must_use]
pub fn intake_key(instance_id: &str) -> DocumentKey {
    DocumentKey {
        scope_type: PLUGIN_INSTANCE_SCOPE_TYPE.to_string(),
        scope_id: instance_id.to_string(),
        namespace: INTAKE_NAMESPACE.to_string(),
    }
}

/// Loads one instance's role document.
///
/// # Returns
///
/// The stored document, or `None` when nobody wrote one: the instance is
/// then a destination.
///
/// # Errors
///
/// Returns the read error, or an error when the stored document is invalid
/// or of an unknown schema.
pub async fn load_intake_role(
    store: &LibraryStore,
    instance_id: &str,
) -> Result<Option<ConfigurationDocument<IntakeRoleV1>>> {
    load_document(store, &intake_key(instance_id))
        .await?
        .map(parse_role)
        .transpose()
}

/// The instance's drop folder settings, or `None` for a destination.
///
/// # Errors
///
/// Returns the read or parse error. A caller deciding where acquired books
/// go fails closed on it rather than writing into a drop folder.
pub async fn intake_settings(
    store: &LibraryStore,
    instance_id: &str,
) -> Result<Option<IntakeSettings>> {
    Ok(load_intake_role(store, instance_id)
        .await?
        .and_then(|document| document.body.intake))
}

/// Makes the instance a drop folder with `settings`, changes a drop
/// folder's settings, or (with `None`) makes it a destination again, under
/// compare-and-swap.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - Who is writing, for authorization and the audit log.
/// * `instance_id` - The storage plugin instance.
/// * `settings` - The drop folder's settings, or `None` for a destination.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for settings that fail
/// [`IntakeSettings::validate`], [`LibraryError::Conflict`] when the role
/// would change while the instance is deployed on some node or when the
/// document keeps changing underneath, or the write's error.
pub async fn set_intake(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    settings: Option<IntakeSettings>,
) -> Result<ConfigurationDocument<IntakeRoleV1>> {
    if let Some(settings) = &settings {
        settings
            .validate()
            .map_err(|err| LibraryError::Invalid(err.to_string()))?;
    }
    let body = IntakeRoleV1 { intake: settings };
    let key = intake_key(instance_id.as_str());
    for attempt in 0..SET_ATTEMPTS {
        let current = match load_intake_role(store, instance_id.as_str()).await? {
            Some(document) => document,
            None => parse_role(
                import_if_absent(
                    store,
                    actor,
                    &key,
                    INTAKE_SCHEMA_VERSION,
                    &to_json(&IntakeRoleV1::default())?,
                    &format!("core-intake-{}-default", instance_id.as_str()),
                )
                .await?,
            )?,
        };
        if current.body == body {
            return Ok(current);
        }
        if current.body.intake.is_some() != body.intake.is_some() {
            refuse_while_deployed(store, instance_id).await?;
        }
        let operation_id = format!(
            "core-intake-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            INTAKE_SCHEMA_VERSION,
            current.revision,
            &to_json(&body)?,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => return parse_role(stored),
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(format!(
        "the role of storage instance {} kept changing; retry",
        instance_id.as_str()
    )))
}

/// Refuses a role change while any node runs, or is meant to run, the
/// instance: the running guest was spawned under the old role.
async fn refuse_while_deployed(store: &LibraryStore, instance_id: &PluginInstanceId) -> Result<()> {
    let running: Vec<String> = list_deployments_for_instance(store, instance_id)
        .await?
        .into_iter()
        .filter(|deployment| match deployment.desired {
            DesiredState::Present | DesiredState::Placed => true,
            DesiredState::Absent | DesiredState::Unplaced => false,
        })
        .map(|deployment| deployment.node_id)
        .collect();
    if running.is_empty() {
        return Ok(());
    }
    Err(LibraryError::Conflict(format!(
        "storage instance {} is deployed on {}; undeploy it before changing whether it is a drop folder",
        instance_id.as_str(),
        running.join(", ")
    )))
}

/// Parses a stored `core.intake` row, failing closed on an unknown schema or
/// settings that do not validate.
fn parse_role(stored: StoredDocument) -> Result<ConfigurationDocument<IntakeRoleV1>> {
    if stored.schema_version != INTAKE_SCHEMA_VERSION {
        return Err(documents::unsupported(
            INTAKE_NAMESPACE,
            stored.schema_version,
        ));
    }
    let invalid = |err: &dyn std::fmt::Display| {
        LibraryError::Invalid(format!("invalid {INTAKE_NAMESPACE}: {err}"))
    };
    let body: StoredRole =
        serde_json::from_str(&stored.document_json).map_err(|err| invalid(&err))?;
    let intake = match body.intake {
        Some(raw) => {
            let bytes = serde_json::to_vec(&raw).map_err(|err| invalid(&err))?;
            Some(parse_intake_settings(&bytes).map_err(|err| invalid(&err))?)
        }
        None => None,
    };
    Ok(document(stored, IntakeRoleV1 { intake }))
}

/// A stored `core.intake` body as it is read back; the settings go through
/// their strict reader.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRole {
    /// The settings object, or `null`; read again by the strict reader.
    intake: Option<serde_json::Value>,
}

/// Serializes a body for storage.
fn to_json(body: &IntakeRoleV1) -> Result<String> {
    serde_json::to_string(body)
        .map_err(|err| LibraryError::Invalid(format!("{INTAKE_NAMESPACE} body: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::{create_plugin_instance, ensure_plugin_deployment};

    /// An operator actor for writes.
    fn operator() -> ConfigActor {
        ConfigActor::Operator {
            id: "operator:test".into(),
        }
    }

    #[tokio::test]
    async fn an_unwritten_instance_is_a_destination() {
        let store = crate::control_plane::tests::memory_store().await;
        assert!(load_intake_role(&store, "a").await.unwrap().is_none());
        assert!(intake_settings(&store, "a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_role_changes_only_while_undeployed() {
        let store = crate::control_plane::tests::memory_store().await;
        let instance = create_plugin_instance(&store, &operator(), "local", "Drop folder")
            .await
            .unwrap();
        let first = set_intake(
            &store,
            &operator(),
            &instance.id,
            Some(IntakeSettings::for_patron(2)),
        )
        .await
        .unwrap();
        assert_eq!(first.body.intake, Some(IntakeSettings::for_patron(2)));
        let again = set_intake(
            &store,
            &operator(),
            &instance.id,
            Some(IntakeSettings::for_patron(2)),
        )
        .await
        .unwrap();
        assert_eq!(again.revision, first.revision, "unchanged is not rewritten");
        ensure_plugin_deployment(
            &store,
            &operator(),
            &instance.id,
            "node-a",
            DesiredState::Present,
        )
        .await
        .unwrap();
        // Settings may change while it runs; the role may not.
        let moved = set_intake(
            &store,
            &operator(),
            &instance.id,
            Some(IntakeSettings {
                settle_seconds: 30,
                ..IntakeSettings::for_patron(2)
            }),
        )
        .await
        .unwrap();
        assert!(moved.revision > first.revision);
        let err = set_intake(&store, &operator(), &instance.id, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Conflict(_)), "{err}");
        assert_eq!(
            intake_settings(&store, instance.id.as_str())
                .await
                .unwrap()
                .map(|settings| settings.settle_seconds),
            Some(30)
        );
    }

    #[tokio::test]
    async fn members_and_bad_settings_are_refused() {
        let store = crate::control_plane::tests::memory_store().await;
        let instance = create_plugin_instance(&store, &operator(), "local", "Drop folder")
            .await
            .unwrap();
        let member = ConfigActor::Member {
            id: "user:9".into(),
        };
        assert!(set_intake(
            &store,
            &member,
            &instance.id,
            Some(IntakeSettings::for_patron(9))
        )
        .await
        .is_err());
        let err = set_intake(
            &store,
            &operator(),
            &instance.id,
            Some(IntakeSettings {
                patron: None,
                ..IntakeSettings::for_patron(1)
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
        assert!(intake_settings(&store, instance.id.as_str())
            .await
            .unwrap()
            .is_none());
    }
}
