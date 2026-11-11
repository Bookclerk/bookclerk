//! Summaries for the plugin cold-start measurement (`tests/cold_start.rs`).
//!
//! The measurement spawns each staged guest several times through the
//! production path and records how long each phase took. This module turns
//! those samples into per-plugin and per-group figures and renders them,
//! sans-IO, so the unit tests drive the same code the measurement does.
//!
//! The figures feed the on-demand activation decision in
//! `docs/adr/event-delivery-model.md`: how long a family takes from "no
//! process" to "answered its first call" decides whether an idle instance
//! of that family may sleep.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

/// One spawn of one guest, measured phase by phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupSample {
    /// From the spawn call to a session whose Cap'n Proto handshake
    /// finished (process start, jail, runtime boot, `open`).
    pub spawn: Duration,
    /// The first `describe` round trip on that session.
    pub describe: Duration,
    /// The first entrypoint call (`health`), when the guest has one the
    /// measurement can make without credentials; `None` otherwise.
    pub first_call: Option<Duration>,
}

impl StartupSample {
    /// Time until the guest could serve a call: spawn plus `describe`.
    ///
    /// # Returns
    ///
    /// `spawn + describe`.
    #[must_use]
    pub fn ready(&self) -> Duration {
        self.spawn + self.describe
    }
}

/// Figures for a set of samples, in milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct StartupStats {
    /// Samples summarized.
    pub runs: usize,
    /// `ready` of the first sample: the coldest start (file cache and
    /// runtime caches not yet warm).
    pub first_ready_ms: f64,
    /// Median `ready` across all samples.
    pub median_ready_ms: f64,
    /// Largest `ready` across all samples.
    pub max_ready_ms: f64,
    /// Median `spawn` across all samples.
    pub median_spawn_ms: f64,
    /// Median first entrypoint call, over the samples that made one.
    pub median_first_call_ms: Option<f64>,
}

/// Milliseconds in `d`, with sub-millisecond precision.
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Median of `values` (the mean of the middle two for an even count).
///
/// # Returns
///
/// `None` for an empty slice.
#[must_use]
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    })
}

/// Summarizes `samples`, the first being the coldest.
///
/// # Returns
///
/// `None` when `samples` is empty.
#[must_use]
pub fn summarize(samples: &[StartupSample]) -> Option<StartupStats> {
    let first = samples.first()?;
    let ready: Vec<f64> = samples.iter().map(|s| ms(s.ready())).collect();
    let spawn: Vec<f64> = samples.iter().map(|s| ms(s.spawn)).collect();
    let calls: Vec<f64> = samples
        .iter()
        .filter_map(|s| s.first_call.map(ms))
        .collect();
    Some(StartupStats {
        runs: samples.len(),
        first_ready_ms: ms(first.ready()),
        median_ready_ms: median(&ready)?,
        max_ready_ms: ready.iter().copied().fold(0.0, f64::max),
        median_spawn_ms: median(&spawn)?,
        median_first_call_ms: median(&calls),
    })
}

/// One measured guest and how it is grouped in the report.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginStartup {
    /// Plugin manifest id.
    pub id: String,
    /// How the guest runs: `workerd/js`, `workerd/python`, or `native` plus
    /// its launcher (`native/node`, `native/python`, `native/binary`).
    pub runtime: String,
    /// Plugin families from the manifest (`source`, `output`, `database`,
    /// `integration`).
    pub families: Vec<String>,
    /// The guest's figures.
    pub stats: StartupStats,
}

/// Groups `plugins` by `key` and takes the median of each member's median
/// `ready` (and first call).
///
/// # Arguments
///
/// * `plugins` - Measured guests.
/// * `keys` - The group keys of one guest (a guest in several families
///   counts in each).
///
/// # Returns
///
/// Group key → (members, median ready ms, worst max ready ms, median first
/// call ms).
#[must_use]
pub fn group_by<F>(plugins: &[PluginStartup], keys: F) -> BTreeMap<String, GroupStats>
where
    F: Fn(&PluginStartup) -> Vec<String>,
{
    let mut members: BTreeMap<String, Vec<&PluginStartup>> = BTreeMap::new();
    for plugin in plugins {
        for key in keys(plugin) {
            members.entry(key).or_default().push(plugin);
        }
    }
    members
        .into_iter()
        .filter_map(|(key, list)| {
            let ready: Vec<f64> = list.iter().map(|p| p.stats.median_ready_ms).collect();
            let calls: Vec<f64> = list
                .iter()
                .filter_map(|p| p.stats.median_first_call_ms)
                .collect();
            Some((
                key,
                GroupStats {
                    members: list.iter().map(|p| p.id.clone()).collect(),
                    median_ready_ms: median(&ready)?,
                    max_ready_ms: list
                        .iter()
                        .map(|p| p.stats.max_ready_ms)
                        .fold(0.0, f64::max),
                    median_first_call_ms: median(&calls),
                },
            ))
        })
        .collect()
}

/// Figures for one group of guests (a family or a runtime).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStats {
    /// Plugin ids in the group.
    pub members: Vec<String>,
    /// Median of the members' median `ready`, in milliseconds.
    pub median_ready_ms: f64,
    /// Largest single `ready` any member took, in milliseconds.
    pub max_ready_ms: f64,
    /// Median of the members' median first call, when any made one.
    pub median_first_call_ms: Option<f64>,
}

/// Renders an optional figure, `-` when absent.
fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:.0}"))
}

/// Renders the report as Markdown: per plugin, per family, per runtime.
///
/// # Arguments
///
/// * `plugins` - Measured guests, in report order.
/// * `context` - One line on where it ran (CPU limit, host), shown first.
///
/// # Returns
///
/// The Markdown text.
#[must_use]
pub fn markdown_report(plugins: &[PluginStartup], context: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Plugin cold start\n\n{context}\n");
    let _ = writeln!(
        out,
        "Times in ms. Ready = spawn (process, jail, runtime, handshake) + first `describe`.\n"
    );
    let _ = writeln!(
        out,
        "| Plugin | Runtime | Families | Runs | First ready | Median ready | Max ready | Median spawn | Median first call |"
    );
    let _ = writeln!(
        out,
        "| --- | --- | --- | --: | --: | --: | --: | --: | --: |"
    );
    for p in plugins {
        let s = &p.stats;
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {} |",
            p.id,
            p.runtime,
            p.families.join(", "),
            s.runs,
            s.first_ready_ms,
            s.median_ready_ms,
            s.max_ready_ms,
            s.median_spawn_ms,
            opt(s.median_first_call_ms),
        );
    }
    for (title, groups) in [
        ("family", group_by(plugins, |p| p.families.clone())),
        ("runtime", group_by(plugins, |p| vec![p.runtime.clone()])),
    ] {
        let _ = writeln!(
            out,
            "\n## By {title}\n\n| {title} | Plugins | Median ready | Max ready | Median first call |\n| --- | --: | --: | --: | --: |"
        );
        for (key, g) in groups {
            let _ = writeln!(
                out,
                "| {key} | {} | {:.0} | {:.0} | {} |",
                g.members.len(),
                g.median_ready_ms,
                g.max_ready_ms,
                opt(g.median_first_call_ms),
            );
        }
    }
    out
}

/// Renders the report as JSON for later comparison.
///
/// # Returns
///
/// A JSON value with one object per plugin.
#[must_use]
pub fn json_report(plugins: &[PluginStartup], context: &str) -> serde_json::Value {
    serde_json::json!({
        "context": context,
        "plugins": plugins.iter().map(|p| serde_json::json!({
            "id": p.id,
            "runtime": p.runtime,
            "families": p.families,
            "runs": p.stats.runs,
            "first_ready_ms": p.stats.first_ready_ms,
            "median_ready_ms": p.stats.median_ready_ms,
            "max_ready_ms": p.stats.max_ready_ms,
            "median_spawn_ms": p.stats.median_spawn_ms,
            "median_first_call_ms": p.stats.median_first_call_ms,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(spawn: u64, describe: u64, call: Option<u64>) -> StartupSample {
        StartupSample {
            spawn: Duration::from_millis(spawn),
            describe: Duration::from_millis(describe),
            first_call: call.map(Duration::from_millis),
        }
    }

    #[test]
    fn median_handles_odd_even_and_empty() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn summarize_keeps_the_cold_run_apart() {
        assert_eq!(summarize(&[]), None);
        let stats = summarize(&[
            sample(900, 100, None),
            sample(90, 10, Some(4)),
            sample(110, 10, Some(6)),
        ])
        .unwrap();
        assert_eq!(stats.runs, 3);
        assert!((stats.first_ready_ms - 1000.0).abs() < 1e-6);
        assert!((stats.median_ready_ms - 120.0).abs() < 1e-6);
        assert!((stats.max_ready_ms - 1000.0).abs() < 1e-6);
        assert!((stats.median_spawn_ms - 110.0).abs() < 1e-6);
        assert_eq!(stats.median_first_call_ms, Some(5.0));
    }

    fn plugin(id: &str, runtime: &str, families: &[&str], ready: u64) -> PluginStartup {
        PluginStartup {
            id: id.into(),
            runtime: runtime.into(),
            families: families.iter().map(|f| (*f).to_string()).collect(),
            stats: summarize(&[sample(ready, 0, None)]).unwrap(),
        }
    }

    #[test]
    fn groups_count_a_plugin_in_each_family() {
        let plugins = [
            plugin("a", "workerd", &["source"], 100),
            plugin("b", "native/binary", &["source", "integration"], 300),
            plugin("c", "workerd", &["integration"], 200),
        ];
        let families = group_by(&plugins, |p| p.families.clone());
        assert_eq!(families["source"].members, vec!["a", "b"]);
        assert!((families["source"].median_ready_ms - 200.0).abs() < 1e-6);
        assert!((families["integration"].max_ready_ms - 300.0).abs() < 1e-6);
        assert_eq!(families["integration"].median_first_call_ms, None);
        let runtimes = group_by(&plugins, |p| vec![p.runtime.clone()]);
        assert_eq!(runtimes["workerd"].members.len(), 2);

        let md = markdown_report(&plugins, "test host");
        assert!(md.contains("| `b` | native/binary | source, integration | 1 |"));
        assert!(md.contains("## By family"));
        assert!(md.contains("| workerd | 2 | 150 |"));
        let json = json_report(&plugins, "test host");
        assert_eq!(json["plugins"][2]["id"], "c");
    }
}
