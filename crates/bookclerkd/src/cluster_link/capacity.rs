//! Capacity shares: how a branch of several libraries splits its machine
//! between its own work and each library it joined.
//!
//! The machine's job slots come from this node's own heartbeat
//! ([`machine_slots`]): its CPU and memory budget (`node.runtime`) over
//! what one guest is assumed to need, and none while the operator drained
//! the node. The branch's own work comes first: every job its own workers
//! run takes a slot ([`CapacityShares::own_job`]). The slots left are
//! dealt out one at a time, in cluster id order, to each active link that
//! is still under its operator-set cap ([`split_slots`]), so links get
//! equal shares and a capped link's unused slots go to the others.
//!
//! Each link's client gets its share as a [`watch::Receiver`]: its claim
//! loop holds no more jobs than that (a smaller share cancels nothing, it
//! only stops claims until enough jobs finish), and its heartbeat reports
//! the library only that part of the machine's CPU and memory
//! ([`ShareSampler`]), so the library's scheduler never counts on the
//! whole machine.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bookclerk_library::control_plane::{
    LinkJobCap, NodeHeartbeat, NodeSampler, NodeStatus, DEFAULT_GUEST_CPU_MILLICORES,
    DEFAULT_GUEST_MEMORY_BYTES,
};
use bookclerk_node_client::HeartbeatSource;
use tokio::sync::watch;

/// The most job slots a machine is counted as having, however large it
/// is; also the most jobs one link's client holds.
pub const MAX_MACHINE_SLOTS: usize = 16;

/// Slots the machine is counted as having before this node's first
/// heartbeat (the two jobs a link held before shares existed).
pub const SLOTS_BEFORE_FIRST_HEARTBEAT: usize = 2;

/// The job slots this node's heartbeat says the machine has: its CPU
/// budget over [`DEFAULT_GUEST_CPU_MILLICORES`] and its memory budget over
/// [`DEFAULT_GUEST_MEMORY_BYTES`], whichever is smaller, from one to
/// [`MAX_MACHINE_SLOTS`]. A drained node (`schedulable = false`) has none,
/// so its libraries stop handing it work too.
#[must_use]
pub fn machine_slots(status: &NodeStatus) -> usize {
    if !status.schedulable {
        return 0;
    }
    let by_cpu = status.cpu_budget_millicores / DEFAULT_GUEST_CPU_MILLICORES;
    let slots = match status.memory_budget_bytes {
        Some(memory) => by_cpu.min(memory / DEFAULT_GUEST_MEMORY_BYTES),
        None => by_cpu,
    };
    usize::try_from(slots)
        .unwrap_or(0)
        .clamp(1, MAX_MACHINE_SLOTS)
}

/// Splits `machine` slots: `own_busy` go to the branch's own work first,
/// and the rest are dealt out one at a time, in the order of `caps`, to
/// each link still under its cap (`None` is uncapped).
///
/// # Returns
///
/// Each link's share, in the order of `caps`. The shares never add up to
/// more than `machine - own_busy`.
#[must_use]
pub fn split_slots(machine: usize, own_busy: usize, caps: &[Option<usize>]) -> Vec<usize> {
    let mut shares = vec![0; caps.len()];
    let mut left = machine.saturating_sub(own_busy);
    while left > 0 {
        let mut dealt = false;
        for (share, cap) in shares.iter_mut().zip(caps) {
            if left == 0 {
                break;
            }
            if cap.is_none_or(|cap| *share < cap) {
                *share += 1;
                left -= 1;
                dealt = true;
            }
        }
        if !dealt {
            break;
        }
    }
    shares
}

/// One link's entry in the share table.
struct LinkShare {
    /// The client run the entry belongs to; a later run of the same link
    /// replaces it, and only the owning run removes it.
    serial: u64,
    /// The operator's cap, in jobs.
    cap: Option<usize>,
    /// The link's share, as its client reads it.
    share: watch::Sender<usize>,
}

/// The share table's state.
struct State {
    /// Job slots the machine has.
    machine: usize,
    /// Jobs the branch's own workers run now.
    own_busy: usize,
    /// Each running link's entry, by owner cluster id.
    links: BTreeMap<String, LinkShare>,
}

impl State {
    /// Recomputes every link's share and tells the clients whose share
    /// changed.
    fn publish(&self) {
        let caps: Vec<Option<usize>> = self.links.values().map(|link| link.cap).collect();
        let shares = split_slots(self.machine, self.own_busy, &caps);
        for (link, share) in self.links.values().zip(shares) {
            link.share.send_if_modified(|current| {
                let changed = *current != share;
                *current = share;
                changed
            });
        }
    }
}

/// How this daemon splits the machine between its own work and each
/// library it is a branch of. One per daemon ([`crate::api::AppState::capacity`]).
pub struct CapacityShares {
    /// The table.
    state: Mutex<State>,
}

/// Holds one slot for a job the branch's own workers run; dropping it
/// gives the slot back to the links.
pub struct OwnJob<'a> {
    /// The table the slot was taken from.
    shares: &'a CapacityShares,
}

impl Drop for OwnJob<'_> {
    fn drop(&mut self) {
        let mut state = self.shares.lock();
        state.own_busy = state.own_busy.saturating_sub(1);
        state.publish();
    }
}

/// A running link's entry in the share table, removed when this drops
/// (the client's task returned or was aborted), so the link's slots go to
/// the others.
pub struct ShareEntry {
    /// The table the entry is in.
    shares: Arc<CapacityShares>,
    /// The link.
    cluster_id: String,
    /// The client run that registered it.
    serial: u64,
}

impl ShareEntry {
    /// The entry registered in `shares` for run `serial` of the link to
    /// `cluster_id`.
    #[must_use]
    pub fn new(shares: Arc<CapacityShares>, cluster_id: &str, serial: u64) -> Self {
        Self {
            shares,
            cluster_id: cluster_id.to_owned(),
            serial,
        }
    }
}

impl Drop for ShareEntry {
    fn drop(&mut self) {
        self.shares.unregister(&self.cluster_id, self.serial);
    }
}

impl CapacityShares {
    /// An empty table with [`SLOTS_BEFORE_FIRST_HEARTBEAT`] slots.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                machine: SLOTS_BEFORE_FIRST_HEARTBEAT,
                own_busy: 0,
                links: BTreeMap::new(),
            }),
        }
    }

    /// Locks the table, recovering from poisoning (every critical section
    /// leaves it consistent).
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds run `serial` of the link to `cluster_id` with its cap,
    /// replacing an earlier run's entry.
    ///
    /// # Returns
    ///
    /// The link's share, for its client's claim loop.
    pub fn register(
        &self,
        cluster_id: &str,
        serial: u64,
        cap: Option<LinkJobCap>,
    ) -> watch::Receiver<usize> {
        let (share, receiver) = watch::channel(0);
        let mut state = self.lock();
        state.links.insert(
            cluster_id.to_owned(),
            LinkShare {
                serial,
                cap: cap.map(LinkJobCap::jobs),
                share,
            },
        );
        state.publish();
        receiver
    }

    /// Removes run `serial` of the link to `cluster_id`; a later run's
    /// entry stays. Its slots go to the other links.
    pub fn unregister(&self, cluster_id: &str, serial: u64) {
        let mut state = self.lock();
        if state
            .links
            .get(cluster_id)
            .is_some_and(|link| link.serial == serial)
        {
            state.links.remove(cluster_id);
            state.publish();
        }
    }

    /// Sets the cap of the running link to `cluster_id`, when it runs.
    pub fn set_cap(&self, cluster_id: &str, cap: Option<LinkJobCap>) {
        let mut state = self.lock();
        if let Some(link) = state.links.get_mut(cluster_id) {
            link.cap = cap.map(LinkJobCap::jobs);
            state.publish();
        }
    }

    /// Sets the job slots the machine has (from [`machine_slots`]).
    pub fn set_machine(&self, slots: usize) {
        let mut state = self.lock();
        if state.machine != slots {
            state.machine = slots;
            state.publish();
        }
    }

    /// Takes a slot for a job the branch's own workers run, until the
    /// returned guard drops.
    #[must_use]
    pub fn own_job(&self) -> OwnJob<'_> {
        let mut state = self.lock();
        state.own_busy = state.own_busy.saturating_add(1);
        state.publish();
        OwnJob { shares: self }
    }

    /// The share of the running link to `cluster_id`, in jobs; `None`
    /// while no client runs for it.
    #[must_use]
    pub fn share_of(&self, cluster_id: &str) -> Option<usize> {
        self.lock()
            .links
            .get(cluster_id)
            .map(|link| *link.share.borrow())
    }

    /// The job slots the machine has now.
    #[must_use]
    pub fn machine(&self) -> usize {
        self.lock().machine
    }
}

impl Default for CapacityShares {
    fn default() -> Self {
        Self::new()
    }
}

/// `value` scaled to `share` of `machine` slots, rounded down; zero when
/// the machine has no slots.
fn scaled(value: i64, share: usize, machine: usize) -> i64 {
    if machine == 0 {
        return 0;
    }
    let share = i128::try_from(share.min(machine)).unwrap_or(0);
    let machine = i128::try_from(machine).unwrap_or(1).max(1);
    i64::try_from(i128::from(value) * share / machine).unwrap_or(0)
}

/// A link's heartbeat source: this node's measurements, with CPU and memory
/// scaled to the link's share of the machine, so the library sees only the
/// part it may use. Free disk is the machine's (the libraries share the
/// files directory's filesystem, not slices of it).
pub struct ShareSampler {
    /// The node's own measurements.
    inner: NodeSampler,
    /// The link's share, in slots.
    share: watch::Receiver<usize>,
    /// The table the machine's slots are read from.
    shares: Arc<CapacityShares>,
}

impl ShareSampler {
    /// A sampler for the link whose share `share` reports.
    #[must_use]
    pub fn new(
        inner: NodeSampler,
        share: watch::Receiver<usize>,
        shares: Arc<CapacityShares>,
    ) -> Self {
        Self {
            inner,
            share,
            shares,
        }
    }
}

/// `sample` scaled to `share` of `machine` slots (see [`ShareSampler`]).
#[must_use]
pub fn scale_heartbeat(sample: NodeHeartbeat, share: usize, machine: usize) -> NodeHeartbeat {
    NodeHeartbeat {
        cpu_millicores: scaled(sample.cpu_millicores, share, machine),
        memory_bytes: sample
            .memory_bytes
            .map(|value| scaled(value, share, machine)),
        cpu_used_millicores: sample
            .cpu_used_millicores
            .map(|value| scaled(value, share, machine)),
        memory_available_bytes: sample
            .memory_available_bytes
            .map(|value| scaled(value, share, machine)),
        ..sample
    }
}

impl HeartbeatSource for ShareSampler {
    fn sample(&mut self) -> NodeHeartbeat {
        let sample = self.inner.sample();
        let share = *self.share.borrow();
        scale_heartbeat(sample, share, self.shares.machine())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn links_get_equal_shares_after_own_work() {
        assert_eq!(split_slots(4, 0, &[None, None]), vec![2, 2]);
        assert_eq!(split_slots(5, 0, &[None, None]), vec![3, 2]);
        assert_eq!(split_slots(4, 1, &[None, None]), vec![2, 1]);
        assert_eq!(split_slots(4, 4, &[None, None]), vec![0, 0]);
        assert_eq!(split_slots(2, 5, &[None]), vec![0]);
        assert_eq!(split_slots(0, 0, &[None, None]), vec![0, 0]);
        assert!(split_slots(3, 0, &[]).is_empty());
    }

    #[test]
    fn a_capped_links_unused_slots_go_to_the_others() {
        assert_eq!(split_slots(6, 0, &[Some(1), None, None]), vec![1, 3, 2]);
        assert_eq!(split_slots(6, 0, &[Some(0), None]), vec![0, 6]);
        assert_eq!(split_slots(6, 0, &[Some(1), Some(2)]), vec![1, 2]);
        assert_eq!(split_slots(3, 0, &[Some(5), None]), vec![2, 1]);
    }

    /// The shares never exceed what own work leaves, and a link never
    /// exceeds its cap, for every small machine.
    #[test]
    fn shares_respect_the_machine_and_every_cap() {
        let caps_list: [&[Option<usize>]; 4] = [
            &[None],
            &[Some(0), None, Some(2)],
            &[Some(1), Some(1)],
            &[None, None, None, Some(3)],
        ];
        for machine in 0..=MAX_MACHINE_SLOTS {
            for own in 0..=machine + 1 {
                for caps in caps_list {
                    let shares = split_slots(machine, own, caps);
                    let left = machine.saturating_sub(own);
                    let total: usize = shares.iter().sum();
                    assert!(total <= left);
                    for (share, cap) in shares.iter().zip(caps.iter()) {
                        assert!(cap.is_none_or(|cap| *share <= cap));
                    }
                    let room = caps.iter().any(Option::is_none)
                        || caps.iter().flatten().sum::<usize>() > total;
                    if room {
                        assert_eq!(total, left, "slots left idle: {machine} {own} {caps:?}");
                    }
                    let uncapped: Vec<_> = shares
                        .iter()
                        .zip(caps.iter())
                        .filter(|(_, cap)| cap.is_none())
                        .map(|(share, _)| *share)
                        .collect();
                    if let (Some(max), Some(min)) = (uncapped.iter().max(), uncapped.iter().min()) {
                        assert!(max - min <= 1, "unequal shares {shares:?}");
                    }
                }
            }
        }
    }

    fn status(cpu_budget: i64, memory_budget: Option<i64>, schedulable: bool) -> NodeStatus {
        NodeStatus {
            node_id: "n".into(),
            incarnation: String::new(),
            started_at: String::new(),
            heartbeat_at: String::new(),
            heartbeat_ms: 0,
            interval_ms: 15_000,
            cpu_millicores: cpu_budget,
            memory_bytes: memory_budget,
            cpu_budget_millicores: cpu_budget,
            memory_budget_bytes: memory_budget,
            schedulable,
            cpu_used_millicores: None,
            memory_available_bytes: None,
            files_dir_free_bytes: None,
            reserved: bookclerk_library::control_plane::GuestReservation::default(),
            running_guests: 0,
            availability_ppm: 0,
            online_since_ms: 0,
            cache_bytes: None,
            cache_budget_bytes: None,
            cache_swept_ms: None,
        }
    }

    #[test]
    fn the_machine_counts_what_its_budget_fits() {
        let gib = 1024 * 1024 * 1024;
        // Four cores at 80%: four guests by CPU, eight by memory.
        assert_eq!(machine_slots(&status(3_200, Some(4 * gib), true)), 4);
        // Memory is the tighter limit.
        assert_eq!(machine_slots(&status(6_400, Some(gib), true)), 2);
        // Memory unknown: CPU alone.
        assert_eq!(machine_slots(&status(1_600, None, true)), 2);
        // A tiny machine still runs one job.
        assert_eq!(machine_slots(&status(400, Some(gib), true)), 1);
        // A large one is capped.
        assert_eq!(
            machine_slots(&status(1_000_000, Some(1024 * gib), true)),
            MAX_MACHINE_SLOTS
        );
        // A drained node takes no work.
        assert_eq!(machine_slots(&status(3_200, Some(4 * gib), false)), 0);
    }

    #[test]
    fn a_link_hears_its_share_change() {
        let table = CapacityShares::new();
        table.set_machine(4);
        let a = table.register("a", 1, None);
        assert_eq!(*a.borrow(), 4);
        let b = table.register("b", 2, None);
        assert_eq!((*a.borrow(), *b.borrow()), (2, 2));
        {
            let _own = table.own_job();
            assert_eq!((*a.borrow(), *b.borrow()), (2, 1));
        }
        assert_eq!((*a.borrow(), *b.borrow()), (2, 2));
        table.set_cap("a", Some(LinkJobCap::new(1).unwrap()));
        assert_eq!((*a.borrow(), *b.borrow()), (1, 3));
        assert_eq!(table.share_of("a"), Some(1));
        assert_eq!(table.share_of("nope"), None);
        // A cap for a link that is not running is ignored.
        table.set_cap("nope", None);
        // A stale run's removal leaves the current entry.
        let b2 = table.register("b", 3, None);
        table.unregister("b", 2);
        assert_eq!(*b2.borrow(), 3);
        table.unregister("b", 3);
        assert_eq!(*a.borrow(), 1);
        table.set_cap("a", None);
        assert_eq!(*a.borrow(), 4);
        table.set_machine(0);
        assert_eq!(*a.borrow(), 0);
    }

    #[test]
    fn a_library_hears_only_its_share_of_the_machine() {
        let sample = NodeHeartbeat {
            incarnation: "i".into(),
            started_at: "s".into(),
            interval: Duration::from_secs(15),
            cpu_millicores: 8_000,
            memory_bytes: Some(16_000),
            cpu_used_millicores: Some(1_000),
            memory_available_bytes: Some(12_000),
            files_dir_free_bytes: Some(5),
            cache: None,
        };
        let half = scale_heartbeat(sample.clone(), 2, 4);
        assert_eq!(half.cpu_millicores, 4_000);
        assert_eq!(half.memory_bytes, Some(8_000));
        assert_eq!(half.cpu_used_millicores, Some(500));
        assert_eq!(half.memory_available_bytes, Some(6_000));
        assert_eq!(half.files_dir_free_bytes, Some(5));
        let none = scale_heartbeat(sample.clone(), 0, 4);
        assert_eq!(none.cpu_millicores, 0);
        assert_eq!(none.memory_bytes, Some(0));
        assert_eq!(scale_heartbeat(sample.clone(), 3, 0).cpu_millicores, 0);
        assert_eq!(scale_heartbeat(sample, 9, 4).cpu_millicores, 8_000);
    }
}
