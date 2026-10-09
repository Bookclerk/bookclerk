//! Selects `Atomic*::try_update` when this rustc has it.
//!
//! The workspace MSRV is 1.94, which only has `fetch_update`. Current stable
//! deprecates that name. Clippy on CI denies the deprecation, so each toolchain
//! compiles exactly one of the two calls. A failed version probe leaves the
//! cfg unset so the 1.94 path is the one that compiles.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(atomic_try_update)");
    if rustc_version().is_some_and(|version| version >= (1, 95)) {
        println!("cargo:rustc-cfg=atomic_try_update");
    }
}

/// `(major, minor)` from `rustc --version`.
///
/// `None` when the compiler cannot be run or its version line cannot be
/// parsed. Callers then leave `cfg(atomic_try_update)` unset.
fn rustc_version() -> Option<(u32, u32)> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_rustc_version(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `rustc 1.95.0 ...` into `(major, minor)`.
fn parse_rustc_version(text: &str) -> Option<(u32, u32)> {
    let version = text.split_whitespace().nth(1)?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}
