//! Routing work for a plugin instance to the best live host that runs it.
//!
//! Hosts never talk to each other, so nobody hands a call to a host. Each
//! host that runs an instance ranks every live host running it, from the
//! heartbeats in the shared database, by the same headroom score placement
//! uses (room for more of this instance's demand, weighted by
//! availability). Every host computes the same ranking. The best host
//! (rank 0) claims the instance's deliveries as soon as they are due; a host
//! ranked `r` claims one only after it has been due for
//! `r × ROUTE_DEFER_PER_RANK`. A busy, stalled, or vanished best host
//! therefore costs at most a short delay before the next one takes over,
//! with no coordination beyond the claim's compare-and-swap.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::Utc;

use super::deployments::{list_deployments_for_instance, load_observation, DeploymentStatus};
use super::host_status::{instance_demand, list_host_statuses, GuestReservation};
use super::identity::list_hosts;
use super::instances::PluginInstanceId;
use super::placement::HostCandidate;
use crate::error::Result;
use crate::store::LibraryStore;

/// How long each rank waits behind the one above it.
pub const ROUTE_DEFER_PER_RANK: Duration = Duration::from_secs(10);

/// This host's position among the live hosts running one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRank {
    /// 0 for the best host.
    pub rank: usize,
    /// Live hosts running the instance, best first.
    pub hosts: Vec<String>,
}

impl RouteRank {
    /// How long this host lets a delivery wait before claiming it.
    #[must_use]
    pub fn defer(&self) -> Duration {
        ROUTE_DEFER_PER_RANK.saturating_mul(u32::try_from(self.rank).unwrap_or(u32::MAX))
    }
}

/// Ranks `running` hosts for `demand`, best first (most room, then host id).
#[must_use]
pub fn rank_hosts(running: &[HostCandidate], demand: GuestReservation) -> Vec<String> {
    let mut ordered: Vec<&HostCandidate> = running.iter().collect();
    ordered.sort_by(|a, b| {
        b.score(demand)
            .cmp(&a.score(demand))
            .then_with(|| a.host_id.cmp(&b.host_id))
    });
    ordered.into_iter().map(|h| h.host_id.clone()).collect()
}

/// For each claim id that is a plugin instance id, `host_id`'s rank among
/// the live hosts whose guest of that instance is up (`running` or
/// `healthy`). Ids that are not instance ids (plugin keys, aliases, host
/// consumers) are left out: they are not routed. A host missing from the
/// list (no heartbeat yet) ranks after every live host.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn route_ranks(
    store: &LibraryStore,
    host_id: &str,
    claim_ids: &[String],
) -> Result<BTreeMap<String, RouteRank>> {
    let instances: BTreeSet<PluginInstanceId> = claim_ids
        .iter()
        .filter_map(|id| PluginInstanceId::parse(id).ok())
        .collect();
    let mut ranks = BTreeMap::new();
    if instances.is_empty() {
        return Ok(ranks);
    }
    let now_ms = Utc::now().timestamp_millis();
    let statuses: BTreeMap<String, _> = list_host_statuses(store)
        .await?
        .into_iter()
        .map(|status| (status.host_id.clone(), status))
        .collect();
    let candidates: BTreeMap<String, HostCandidate> = list_hosts(store)
        .await?
        .iter()
        .map(|host| {
            (
                host.host_id.clone(),
                HostCandidate::from_status(host, statuses.get(&host.host_id), now_ms),
            )
        })
        .filter(|(_, candidate)| candidate.live)
        .collect();
    for instance in instances {
        let mut running = Vec::new();
        for deployment in list_deployments_for_instance(store, &instance).await? {
            if !deployment.desired.runs() {
                continue;
            }
            let Some(candidate) = candidates.get(&deployment.host_id) else {
                continue;
            };
            let up = load_observation(store, &deployment.deployment_id, &deployment.host_id)
                .await?
                .is_some_and(|o| {
                    matches!(
                        o.status,
                        DeploymentStatus::Running | DeploymentStatus::Healthy
                    )
                });
            if up {
                running.push(candidate.clone());
            }
        }
        let demand = instance_demand(store, &instance)
            .await?
            .unwrap_or_else(GuestReservation::default_guest);
        let hosts = rank_hosts(&running, demand);
        let rank = hosts
            .iter()
            .position(|id| id == host_id)
            .unwrap_or(hosts.len());
        ranks.insert(instance.as_str().to_string(), RouteRank { rank, hosts });
    }
    Ok(ranks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, cores: i64, availability_ppm: i64) -> HostCandidate {
        HostCandidate {
            host_id: id.into(),
            labels: BTreeSet::new(),
            target: String::new(),
            live: true,
            schedulable: true,
            cpu_headroom_millicores: cores * 1000,
            memory_headroom_bytes: Some(8 << 30),
            availability_ppm,
        }
    }

    #[test]
    fn the_host_with_the_most_room_ranks_first_and_waits_least() {
        let order = rank_hosts(
            &[
                candidate("vps", 1, 1_000_000),
                candidate("desk", 10, 400_000),
                candidate("busy", 0, 1_000_000),
            ],
            GuestReservation::default_guest(),
        );
        assert_eq!(order, vec!["desk", "vps", "busy"]);
        let second = RouteRank {
            rank: 1,
            hosts: order,
        };
        assert_eq!(second.defer(), ROUTE_DEFER_PER_RANK);
        assert_eq!(
            RouteRank {
                rank: 0,
                hosts: Vec::new()
            }
            .defer(),
            Duration::ZERO
        );
    }
}
