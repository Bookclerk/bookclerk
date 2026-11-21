//! Probe rustc and the source commit at build time for diagnostics payloads.

/// The commit being built: `BOOKCLERK_BUILD_COMMIT` when the build passes one
/// (container builds, which have no `.git`), else `git rev-parse HEAD`.
/// Anything that is not 7 to 40 hex digits is left out.
fn build_commit() -> Option<String> {
    println!("cargo:rerun-if-env-changed=BOOKCLERK_BUILD_COMMIT");
    let from_env = std::env::var("BOOKCLERK_BUILD_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let raw = match from_env {
        Some(raw) => raw,
        None => {
            // Rebuild when the checkout moves to another commit.
            let git = std::path::Path::new("../../.git");
            if git.join("HEAD").is_file() {
                println!("cargo:rerun-if-changed=../../.git/HEAD");
                if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
                    if let Some(reference) = head.trim().strip_prefix("ref: ") {
                        println!("cargo:rerun-if-changed=../../.git/{reference}");
                    }
                }
            }
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            String::from_utf8(output.stdout).ok()?
        }
    };
    let commit = raw.trim().to_ascii_lowercase();
    ((7..=40).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(commit)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(commit) = build_commit() {
        println!("cargo:rustc-env=BOOKCLERK_BUILD_COMMIT={commit}");
    }
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
