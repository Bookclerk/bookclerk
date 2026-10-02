//! Selects `Atomic*::try_update` when this rustc has it.
//!
//! The workspace MSRV is 1.94, which only has `fetch_update`. Current stable
//! deprecates that name. Clippy on CI denies the deprecation, so each toolchain
//! compiles exactly one of the two calls.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(atomic_try_update)");
    let version = rustc_version();
    if version >= (1, 95) {
        println!("cargo:rustc-cfg=atomic_try_update");
    }
}

/// `(major, minor)` from `rustc --version`.
fn rustc_version() -> (u32, u32) {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .expect("rustc --version");
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().nth(1).unwrap_or("0.0.0");
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    (major, minor)
}
