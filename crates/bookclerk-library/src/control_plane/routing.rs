//! Routing work for a plugin instance to the best live node that runs it.
//!
//! Nodes never talk to each other, so nobody hands a call to a node. Each
//! node that runs an instance ranks every live node running it, from the
//! heartbeats in the shared database, by the same headroom score placement
//! uses (room for more of this instance's demand, weighted by
//! availability). Every node computes the same ranking. The best node
//! (rank 0) claims the instance's deliveries as soon as they are due; a node
//! ranked `r` claims one only after it has been due for
//! `r × ROUTE_DEFER_PER_RANK`. A busy, stalled, or vanished best node
//! therefore costs at most a short delay before the next one takes over,
//! with no coordination beyond the claim's compare-and-swap.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::Utc;

use super::deployments::{list_deployments_for_instance, load_observation, DeploymentStatus};
use super::identity::list_nodes;
use super::instances::PluginInstanceId;
use super::node_status::{instance_demand, list_node_statuses, GuestReservation};
use super::placement::NodeCandidate;
use crate::error::Result;
use crate::store::LibraryStore;

/// How long each rank waits behind the one above it.
pub const ROUTE_DEFER_PER_RANK: Duration = Duration::from_secs(10);

/// This node's position among the live nodes running one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRank {
    /// 0 for the best node.
    pub rank: usize,
    /// Live nodes running the instance, best first.
    pub nodes: Vec<String>,
}

impl RouteRank {
    /// How long this node lets a delivery wait before claiming it.
    #[must_use]
    pub fn defer(&self) -> Duration {
        ROUTE_DEFER_PER_RANK.saturating_mul(u32::try_from(self.rank).unwrap_or(u32::MAX))
    }
}

/// Ranks `running` nodes for `demand`, best first (most room, then node id).
#[must_use]
pub fn rank_nodes(running: &[NodeCandidate], demand: GuestReservation) -> Vec<String> {
    let mut ordered: Vec<&NodeCandidate> = running.iter().collect();
    ordered.sort_by(|a, b| {
        b.score(demand)
            .cmp(&a.score(demand))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    ordered.into_iter().map(|h| h.node_id.clone()).collect()
}

/// For each claim id that is a plugin instance id, `node_id`'s rank among
/// the live nodes whose guest of that instance is up (`running` or
/// `healthy`). Ids that are not instance ids (plugin keys, aliases, node
/// consumers) are left out: they are not routed. A node missing from the
/// list (no heartbeat yet) ranks after every live node.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn route_ranks(
    store: &LibraryStore,
    node_id: &str,
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
    let statuses: BTreeMap<String, _> = list_node_statuses(store)
        .await?
        .into_iter()
        .map(|status| (status.node_id.clone(), status))
        .collect();
    let candidates: BTreeMap<String, NodeCandidate> = list_nodes(store)
        .await?
        .iter()
        .map(|node| {
            (
                node.node_id.clone(),
                NodeCandidate::from_status(node, statuses.get(&node.node_id), now_ms),
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
            let Some(candidate) = candidates.get(&deployment.node_id) else {
                continue;
            };
            let up = load_observation(store, &deployment.deployment_id, &deployment.node_id)
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
        let nodes = rank_nodes(&running, demand);
        let rank = nodes
            .iter()
            .position(|id| id == node_id)
            .unwrap_or(nodes.len());
        ranks.insert(instance.as_str().to_string(), RouteRank { rank, nodes });
    }
    Ok(ranks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, cores: i64, availability_ppm: i64) -> NodeCandidate {
        NodeCandidate {
            node_id: id.into(),
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
    fn the_node_with_the_most_room_ranks_first_and_waits_least() {
        let order = rank_nodes(
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
            nodes: order,
        };
        assert_eq!(second.defer(), ROUTE_DEFER_PER_RANK);
        assert_eq!(
            RouteRank {
                rank: 0,
                nodes: Vec::new()
            }
            .defer(),
            Duration::ZERO
        );
    }
}
