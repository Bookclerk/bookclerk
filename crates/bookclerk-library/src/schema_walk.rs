//! Plan walking for host schema upgrades and explicit `db migrate --to` downs.

use crate::error::{LibraryError, Result};
use crate::migrations::{min_supported_schema_version_in, HostMigrationStep};
use crate::schema_state::SchemaState;

/// Result of walking a host migration plan from one version to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaWalk {
    /// Database version before the walk.
    pub from: i64,
    /// Version the caller asked to reach.
    pub requested_to: i64,
    /// Version the walk can actually reach (may be higher than `requested_to`
    /// when an irreversible step blocks a downgrade).
    pub stopped_at: i64,
    /// Remaining `up` steps in order (empty when rolling back).
    pub ups: Vec<HostMigrationStep>,
    /// `down` steps newest-first (empty when upgrading).
    pub downs: Vec<HostMigrationStep>,
    /// True when a missing `down` stopped a rollback short of `requested_to`.
    pub blocked: bool,
}

impl SchemaWalk {
    /// True when no DDL remains to apply.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.ups.is_empty() && self.downs.is_empty()
    }
}

/// Plans ups or downs from `from` toward `to` using `plan`.
///
/// Downgrades walk newest-first and stop at the last reversible version when
/// the next step has `down: None`. Versions newer than the compiled plan
/// cannot be reversed by this binary. Versions older than the first frozen
/// step in `plan` (except empty `from == 0`) cannot be upgraded by this
/// binary. An empty plan means no frozen schema versions exist.
///
/// # Errors
///
/// Returns [`LibraryError::Schema`] when `to` is below zero, `from` is ahead
/// of every compiled step (unknown newer schema), or `from` is below the
/// plan's first retained frozen version.
pub fn plan_schema_walk(plan: &[HostMigrationStep], from: i64, to: i64) -> Result<SchemaWalk> {
    plan_schema_walk_bounded(plan, from, to, min_supported_schema_version_in(plan))
}

/// Same as [`plan_schema_walk`] with an explicit support floor (for tests).
fn plan_schema_walk_bounded(
    plan: &[HostMigrationStep],
    from: i64,
    to: i64,
    min_supported: Option<i64>,
) -> Result<SchemaWalk> {
    if to < 0 {
        return Err(LibraryError::Schema(
            "cannot migrate to a negative schema version".into(),
        ));
    }
    if let Some(min_supported) = min_supported {
        if from > 0 && from < min_supported {
            return Err(LibraryError::Schema(format!(
                "database schema version {from} is older than this binary supports \
                 ({min_supported}); restore a backup"
            )));
        }
    }
    let max_plan = plan.iter().map(|s| s.version).max().unwrap_or(0);
    if from > max_plan {
        return Err(LibraryError::Schema(format!(
            "database schema version {from} is newer than this binary ({max_plan}); \
             run a newer Bookclerk binary, or restore a backup captured before that freeze"
        )));
    }
    if to > max_plan {
        return Err(LibraryError::Schema(format!(
            "target schema version {to} is newer than this binary ({max_plan})"
        )));
    }

    if to >= from {
        let ups: Vec<HostMigrationStep> = plan
            .iter()
            .copied()
            .filter(|step| step.version > from && step.version <= to)
            .collect();
        let stopped_at = ups.last().map(|s| s.version).unwrap_or(from);
        return Ok(SchemaWalk {
            from,
            requested_to: to,
            stopped_at,
            ups,
            downs: Vec::new(),
            blocked: false,
        });
    }

    let mut downs = Vec::new();
    let mut current = from;
    let mut blocked = false;
    while current > to {
        let Some(step) = plan.iter().find(|s| s.version == current).copied() else {
            return Err(LibraryError::Schema(format!(
                "database schema version {current} is not in this binary's plan; restore a backup"
            )));
        };
        if step.down.is_none() {
            blocked = true;
            break;
        }
        downs.push(step);
        current -= 1;
    }
    Ok(SchemaWalk {
        from,
        requested_to: to,
        stopped_at: current,
        ups: Vec::new(),
        downs,
        blocked,
    })
}

/// Plans a frozen walk from an explicit [`SchemaState`].
///
/// [`SchemaState::Unreleased`] is never treated as empty `from == 0`. A future
/// freeze must not apply version-1 ups on top of an unreleased development
/// database — recreate (`cargo reset --yes`) unless a later explicit promote
/// path is added.
///
/// # Errors
///
/// Returns [`LibraryError::Schema`] when an unreleased database is asked to
/// become a frozen revision, or when [`plan_schema_walk`] fails.
pub fn plan_schema_walk_from_state(
    plan: &[HostMigrationStep],
    state: &SchemaState,
    to: i64,
) -> Result<SchemaWalk> {
    match state {
        SchemaState::Uninitialized => plan_schema_walk(plan, 0, to),
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => {
            if to > 0 {
                return Err(LibraryError::Schema(format!(
                    "unreleased@base{base_version}+{checksum} is not an empty predecessor to frozen {to}; \
                     recreate the database (`cargo reset --yes`) — Bookclerk will not \
                     apply a freeze on top of an unreleased development schema"
                )));
            }
            Ok(SchemaWalk {
                from: 0,
                requested_to: to,
                stopped_at: 0,
                ups: Vec::new(),
                downs: Vec::new(),
                blocked: false,
            })
        }
        SchemaState::Frozen { version, .. } => plan_schema_walk(plan, *version, to),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(version: i64, reversible: bool) -> HostMigrationStep {
        const UP: &[crate::migrations::MigrationOp] =
            &[crate::migrations::MigrationOp::Schema("UP")];
        const DOWN: &[crate::migrations::MigrationOp] =
            &[crate::migrations::MigrationOp::Schema("DOWN")];
        HostMigrationStep {
            version,
            steps: UP,
            down: reversible.then_some(DOWN),
            introduced_in: "0.1.0",
        }
    }

    #[test]
    fn upgrade_selects_intervening_ups() {
        let plan = [step(1, false), step(2, true), step(3, true)];
        let walk = plan_schema_walk(&plan, 1, 3).unwrap();
        assert_eq!(
            walk.ups.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(walk.downs.is_empty());
        assert!(!walk.blocked);
        assert_eq!(walk.stopped_at, 3);
    }

    #[test]
    fn downgrade_walks_newest_first_until_irreversible() {
        let plan = [step(1, false), step(2, true), step(3, true)];
        let walk = plan_schema_walk(&plan, 3, 1).unwrap();
        assert_eq!(
            walk.downs.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![3, 2]
        );
        assert_eq!(walk.stopped_at, 1);
        assert!(!walk.blocked);
    }

    #[test]
    fn downgrade_stops_at_last_reversible_when_blocked() {
        let plan = [step(1, false), step(2, true), step(3, true)];
        let walk = plan_schema_walk(&plan, 3, 0).unwrap();
        assert_eq!(
            walk.downs.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![3, 2]
        );
        assert_eq!(walk.stopped_at, 1);
        assert!(walk.blocked);
        assert_eq!(walk.requested_to, 0);
    }

    #[test]
    fn unknown_newer_schema_fails_closed() {
        let plan = [step(1, false)];
        let err = plan_schema_walk(&plan, 2, 1).unwrap_err();
        assert!(err.to_string().contains("newer than this binary"), "{err}");
    }

    #[test]
    fn noop_when_already_at_target() {
        let plan = [step(1, false)];
        let walk = plan_schema_walk(&plan, 1, 1).unwrap();
        assert!(walk.is_noop());
        assert_eq!(walk.stopped_at, 1);
    }

    #[test]
    fn empty_plan_means_no_frozen_versions() {
        assert_eq!(min_supported_schema_version_in(&[]), None);
        let walk = plan_schema_walk(&[], 0, 0).unwrap();
        assert!(walk.is_noop());
        let err = plan_schema_walk(&[], 1, 1).unwrap_err();
        assert!(err.to_string().contains("newer than this binary"), "{err}");
    }

    #[test]
    fn empty_database_is_allowed_when_min_supported_is_one() {
        let plan = [step(1, false)];
        assert_eq!(min_supported_schema_version_in(&plan), Some(1));
        let walk = plan_schema_walk_bounded(&plan, 0, 1, Some(1)).unwrap();
        assert_eq!(walk.ups.len(), 1);
        assert_eq!(walk.stopped_at, 1);
    }

    #[test]
    fn unreleased_is_not_empty_predecessor_to_frozen() {
        let plan = [step(1, false)];
        let err = plan_schema_walk_from_state(
            &plan,
            &SchemaState::Unreleased {
                base_version: 0,
                checksum: "abc".into(),
            },
            1,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not an empty predecessor"),
            "{err}"
        );
        let noop = plan_schema_walk_from_state(
            &plan,
            &SchemaState::Unreleased {
                base_version: 0,
                checksum: "abc".into(),
            },
            0,
        )
        .unwrap();
        assert!(noop.is_noop());
        let empty = plan_schema_walk_from_state(&plan, &SchemaState::Uninitialized, 1).unwrap();
        assert_eq!(empty.ups.len(), 1);
    }

    #[test]
    fn unreleased_based_on_v1_is_not_empty_predecessor_to_v2() {
        let plan = [step(1, false), step(2, false)];
        let err = plan_schema_walk_from_state(
            &plan,
            &SchemaState::Unreleased {
                base_version: 1,
                checksum: "dev".into(),
            },
            2,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not an empty predecessor"),
            "{err}"
        );
        let frozen = plan_schema_walk_from_state(
            &plan,
            &SchemaState::Frozen {
                version: 1,
                checksum: "f1".into(),
            },
            2,
        )
        .unwrap();
        assert_eq!(frozen.ups.len(), 1);
        assert_eq!(frozen.from, 1);
    }

    #[test]
    fn below_min_supported_fails_closed() {
        let plan = [step(1, false), step(2, true), step(3, true)];
        let err = plan_schema_walk_bounded(&plan, 1, 3, Some(2)).unwrap_err();
        assert!(
            err.to_string().contains("older than this binary supports"),
            "{err}"
        );
    }
}
