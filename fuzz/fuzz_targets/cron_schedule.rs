//! Coverage-guided `[triggers] crons` parsing and fire-time search
//! (`bookclerk-plugin-manifest::CronSchedule`). A cron comes from a
//! third-party `plugin.toml`, and the host scheduler runs the search on it
//! every tick. The oracle in `fuzz/oracles/cron_schedule.rs` is shared with
//! the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/cron_schedule.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
