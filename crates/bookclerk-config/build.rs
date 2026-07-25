//! Probe rustc at build time for diagnostics payloads.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let Ok(output) = std::process::Command::new("rustc").arg("-vV").output() else {
        println!("cargo:rustc-env=BOOKCLERK_RUSTC_RELEASE=unknown");
        println!("cargo:rustc-env=BOOKCLERK_RUSTC_CHANNEL=unknown");
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut release = "unknown".to_string();
    let mut host = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("release: ") {
            release = v.trim().to_string();
        }
        if let Some(v) = line.strip_prefix("host: ") {
            host = v.trim().to_string();
        }
    }
    let channel = if release.contains("nightly") {
        "nightly"
    } else if release.contains("beta") {
        "beta"
    } else {
        "stable"
    };
    println!("cargo:rustc-env=BOOKCLERK_RUSTC_RELEASE={release}");
    println!("cargo:rustc-env=BOOKCLERK_RUSTC_CHANNEL={channel}");
    if !host.is_empty() {
        println!("cargo:rustc-env=BOOKCLERK_RUSTC_HOST={host}");
    }
}
