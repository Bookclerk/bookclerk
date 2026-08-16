//! Spawn-env allowlist for jailed plugin children.
//!
//! The newline-JSON `PluginClient` was removed with `api_version = 1`. Cap'n
//! Proto guests still inherit a filtered environment from [`crate::spawn_stdio`].

/// Env keys safe to inherit into a plugin child.
///
/// Explicitly excludes Bookclerk/AWS/Cloudflare secrets and DB URLs.
///
/// `HOME` and the temp-directory variables are listed because a guest needs
/// *some* value for them, but the inherited one names a path outside the jail.
/// Spawn overwrites all four with the guest's own directories after this filter
/// runs. `XDG_RUNTIME_DIR` is absent for the same reason and has no per-guest
/// equivalent to point at.
pub(crate) fn plugin_env_allowed(key: &str) -> bool {
    const ALLOW: &[&str] = &[
        "PATH",
        "PATHEXT",
        "HOME",
        "USER",
        "USERNAME",
        "USERPROFILE",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "TMPDIR",
        "TEMP",
        "TMP",
        "TERM",
        "COLORTERM",
        "RUST_BACKTRACE",
        "NO_COLOR",
        "FORCE_COLOR",
        // Windows AppContainer launch / DLL load.
        "SystemRoot",
        "SystemDrive",
        "windir",
        "LOCALAPPDATA",
        "ComSpec",
    ];
    if ALLOW.iter().any(|k| key.eq_ignore_ascii_case(k)) {
        return true;
    }
    // Block anything that looks like a secret or Bookclerk bootstrap var.
    let upper = key.to_ascii_uppercase();
    if upper.starts_with("BOOKCLERK_")
        || upper.starts_with("AWS_")
        || upper.starts_with("CLOUDFLARE_")
        || upper.contains("PASSWORD")
        || upper.contains("SECRET")
        || upper.contains("TOKEN")
        || upper.contains("API_KEY")
        || upper.contains("DATABASE_URL")
    {
        return false;
    }
    false
}

#[cfg(test)]
mod env_tests {
    use super::plugin_env_allowed;
    use crate::jail::ensure_plugin_state_within_budget_limit;

    #[test]
    fn allows_path_blocks_secrets() {
        assert!(plugin_env_allowed("PATH"));
        assert!(plugin_env_allowed("HOME"));
        assert!(!plugin_env_allowed("BOOKCLERK_AUTH_PASSWORD"));
        assert!(!plugin_env_allowed("BOOKCLERK_OPERATOR_TOKEN"));
        assert!(!plugin_env_allowed("AWS_SECRET_ACCESS_KEY"));
        assert!(!plugin_env_allowed("CLOUDFLARE_API_TOKEN"));
        assert!(!plugin_env_allowed("BOOKCLERK_DATABASE_POSTGRES_URL"));
    }

    /// Mirrors the spawn/reload disk-budget gate: growth after a lean check must deny the next
    /// plan.
    /// write-capable pass without needing a live guest.
    #[test]
    fn side_pass_budget_gate_denies_after_growth() {
        let root = tempfile::tempdir().expect("tempdir");
        let data = root.path().join("data");
        let scratch = root.path().join("tmp");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();

        ensure_plugin_state_within_budget_limit("echo", &data, &scratch, 32).expect("lean");
        std::fs::write(data.join("blob"), vec![0u8; 64]).unwrap();
        let err = ensure_plugin_state_within_budget_limit("echo", &data, &scratch, 32)
            .expect_err("side-pass must refuse");
        assert!(err.to_string().contains("limit 32"), "got: {err}");
    }
}
