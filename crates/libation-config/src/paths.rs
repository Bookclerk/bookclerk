//! Data-directory resolution with Libation and XDG compatibility.

use std::env;
use std::path::PathBuf;

use directories::ProjectDirs;

/// Environment variable used by upstream Libation / LibationCli / Docker images.
pub const LIBATION_FILES_DIR_ENV: &str = "LIBATION_FILES_DIR";

/// Override for the config file path (`LIBATION_CONFIG`).
pub const LIBATION_CONFIG_ENV: &str = "LIBATION_CONFIG";

/// Resolved filesystem locations for Libation data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Root data directory (accounts, DB, settings).
    pub files_dir: PathBuf,
    /// Default config.toml location inside `files_dir`.
    pub config_file: PathBuf,
    /// SQLite library database path.
    pub library_db: PathBuf,
    /// Temporary download / decrypt scratch space.
    pub cache_dir: PathBuf,
    /// Reserved path (legacy). Logging goes to stderr + journald; Libation does
    /// not write or rotate files here.
    pub log_dir: PathBuf,
    /// Tantivy full-text search index (classic Lucene `SearchEngine`).
    pub search_index_dir: PathBuf,
}

impl Paths {
    /// Build path set from an explicit files directory.
    #[must_use]
    pub fn from_files_dir(files_dir: PathBuf) -> Self {
        let config_file = files_dir.join("config.toml");
        let library_db = files_dir.join("library.db");
        let cache_dir = files_dir.join("cache");
        let log_dir = files_dir.join("logs");
        let search_index_dir = files_dir.join("search_index");
        Self {
            files_dir,
            config_file,
            library_db,
            cache_dir,
            log_dir,
            search_index_dir,
        }
    }

    /// Ensure directories that Libation expects to exist are present.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.files_dir)?;
        std::fs::create_dir_all(&self.cache_dir)?;
        std::fs::create_dir_all(&self.log_dir)?;
        std::fs::create_dir_all(&self.search_index_dir)?;
        std::fs::create_dir_all(self.files_dir.join("Accounts"))?;
        Ok(())
    }
}

/// Resolve the Libation files directory.
///
/// Precedence:
/// 1. Explicit `--libation-files` / caller-provided path
/// 2. `LIBATION_FILES_DIR`
/// 3. XDG data dir: `$XDG_DATA_HOME/libation` (or platform equivalent)
/// 4. Fallback: `./LibationFiles` (cwd-relative, Docker-friendly)
#[must_use]
pub fn resolve_files_dir(cli_override: Option<PathBuf>) -> PathBuf {
    resolve_files_dir_with(cli_override, env::var(LIBATION_FILES_DIR_ENV).ok())
}

/// Testable core of [`resolve_files_dir`].
#[must_use]
pub fn resolve_files_dir_with(
    cli_override: Option<PathBuf>,
    env_files_dir: Option<String>,
) -> PathBuf {
    if let Some(path) = cli_override {
        return path;
    }

    if let Some(path) = env_files_dir {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    if let Some(proj) = ProjectDirs::from("com", "Libation", "libation") {
        return proj.data_dir().to_path_buf();
    }

    PathBuf::from("LibationFiles")
}

/// Resolve config file path: `LIBATION_CONFIG` → `{files_dir}/config.toml`.
#[must_use]
pub fn resolve_config_path(files_dir: &std::path::Path) -> PathBuf {
    if let Ok(path) = env::var(LIBATION_CONFIG_ENV) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    files_dir.join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_override_wins() {
        let path = resolve_files_dir_with(
            Some(PathBuf::from("/tmp/custom-libation")),
            Some("/var/lib/libation".into()),
        );
        assert_eq!(path, PathBuf::from("/tmp/custom-libation"));
    }

    #[test]
    fn env_var_used_when_no_cli_override() {
        let path = resolve_files_dir_with(None, Some("/var/lib/libation".into()));
        assert_eq!(path, PathBuf::from("/var/lib/libation"));
    }

    #[test]
    fn blank_env_falls_through() {
        let path = resolve_files_dir_with(None, Some("   ".into()));
        // Either XDG project dir or cwd fallback — never blank.
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn paths_from_files_dir_layout() {
        let paths = Paths::from_files_dir(PathBuf::from("/data"));
        assert_eq!(paths.config_file, PathBuf::from("/data/config.toml"));
        assert_eq!(paths.library_db, PathBuf::from("/data/library.db"));
        assert_eq!(paths.cache_dir, PathBuf::from("/data/cache"));
        assert_eq!(paths.search_index_dir, PathBuf::from("/data/search_index"));
    }
}
