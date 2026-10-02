//! Choose `Atomic*::try_update` when this rustc is 1.95 or newer.
//!
//! `try_update` is the 1.95 rename of `fetch_update`. Workspace MSRV stays
//! 1.94, so that toolchain compiles `fetch_update`. Current CI denies the
//! deprecation, so 1.95+ compiles `try_update`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(bookclerk_atomic_try_update)");
    if rustc_at_least(1, 95) {
        println!("cargo:rustc-cfg=bookclerk_atomic_try_update");
    }
}

/// True when `rustc -vV` reports a release at or after `major.minor`.
fn rustc_at_least(major: u32, minor: u32) -> bool {
    let Ok(output) = std::process::Command::new("rustc").arg("-vV").output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(release) = text.lines().find_map(|line| line.strip_prefix("release: ")) else {
        return false;
    };
    let mut parts = release.trim().split(['.', '-', ' ']);
    let got_major = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let got_minor = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    (got_major, got_minor) >= (major, minor)
}
