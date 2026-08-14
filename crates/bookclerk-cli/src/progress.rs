//! Terminal progress helpers (LibationCli-style bars when stderr is a TTY).

use std::io::{self, IsTerminal, Write};
use std::time::Instant;

/// True when interactive progress output is appropriate (stderr TTY, not redirected).
#[must_use]
pub fn is_interactive() -> bool {
    io::stderr().is_terminal()
}

/// Simple batch progress: `[####------] 2/5 title`.
pub struct BatchProgress {
    /// Expected item count (clamped to at least 1 so the bar fraction is defined).
    total: usize,
    /// Items completed so far, used for the bar fill and ETA.
    current: usize,
    /// Verb shown after the counts (`scan`, `acquire`, …).
    label: String,
    /// When the bar started; used to estimate remaining minutes.
    started: Instant,
    /// False when stderr is not a TTY (progress is a no-op).
    enabled: bool,
}

impl BatchProgress {
    #[must_use]
    /// Starts a bar for `total` items; disabled automatically when stderr is redirected.
    pub fn new(total: usize, label: impl Into<String>) -> Self {
        Self {
            total: total.max(1),
            current: 0,
            label: label.into(),
            started: Instant::now(),
            enabled: is_interactive(),
        }
    }

    /// Rewrites the current line with bar, counts, detail, and remaining minutes.
    pub fn set(&mut self, current: usize, detail: &str) {
        if !self.enabled {
            return;
        }
        self.current = current;
        let width = 20usize;
        let frac = (current as f64 / self.total as f64).clamp(0.0, 1.0);
        let filled = (frac * width as f64).round() as usize;
        let bar: String = (0..width)
            .map(|i| if i < filled { '#' } else { '-' })
            .collect();
        let eta = if current > 0 && current < self.total {
            let elapsed = self.started.elapsed().as_secs_f64();
            let per = elapsed / current as f64;
            let remain = per * (self.total - current) as f64;
            format!(" {:.1} min remaining", remain / 60.0)
        } else {
            String::new()
        };
        eprint!(
            "\r[{bar}] {current}/{total} {label}: {detail}{eta}   ",
            total = self.total,
            label = self.label,
            detail = detail,
            eta = eta,
        );
        let _ = io::stderr().flush();
    }

    /// Ends the current progress line so later stderr output starts on a new line.
    pub fn finish(&mut self) {
        if self.enabled {
            eprintln!();
        }
    }
}
