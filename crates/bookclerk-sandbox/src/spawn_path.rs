//! Validate program and argv path operands before `Command::new` / `.arg`.
//!
//! These helpers enforce runtime spawn safety properties where a real product
//! boundary exists: no interior NULs, absolute paths (or a single PATH lookup
//! name) when required, existing regular files after canonicalize, containment
//! under a trusted root, or equality with a known helper beside another binary.
//! Paths stay as [`PathBuf`] / [`OsStr`] end-to-end.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Why a spawn path was rejected.
#[derive(Debug, thiserror::Error)]
pub enum SpawnPathError {
    /// Empty `OsStr` / path.
    #[error("spawn path is empty")]
    Empty,
    /// Interior NUL byte (or wide NUL on Windows).
    #[error("spawn path contains an interior NUL")]
    Nul,
    /// Relative path with separators (not a single PATH lookup name).
    #[error("spawn path must be absolute or a single PATH name: {}", .0.display())]
    NotAbsolute(PathBuf),
    /// Absolute path that is not a regular file when one is required.
    #[error("spawn path is not a file: {}", .0.display())]
    NotFile(PathBuf),
    /// Canonical path escapes the trusted root.
    #[error("spawn path {} escapes trusted root {}", .path.display(), .root.display())]
    OutsideRoot {
        /// Candidate path after normalization.
        path: PathBuf,
        /// Trusted directory the path must stay under.
        root: PathBuf,
    },
    /// `canonicalize` failed for an existing path that must resolve.
    #[error("could not canonicalize {}: {source}", .path.display())]
    Canonicalize {
        /// Path that failed to resolve.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

/// [`std::fs::canonicalize`] that also works inside a Windows AppContainer.
///
/// On Windows, `canonicalize` asks the mount manager for the DOS drive letter
/// (`VOLUME_NAME_DOS`), which an AppContainer token may not query, so every
/// path fails with "Access is denied" even when the file is readable. The
/// fallback resolves the volume-relative final path from the open handle,
/// re-attaches the caller's drive, and accepts it only when it opens as the
/// very same file (volume serial + file index); otherwise the original error
/// stands. `GetFinalPathNameByHandleW` follows reparse points; a junction to
/// another volume yields a different identity and falls back to the original
/// error. Callers that later reopen the returned path by name (rather than by
/// the handle used here) still see the usual TOCTOU gap — this helper proves
/// identity at the moment of canonicalize, not at a later open.
///
/// # Errors
///
/// Returns the [`std::fs::canonicalize`] error when the path cannot be
/// resolved (or, on Windows, cannot be proven identical by handle).
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        #[cfg(windows)]
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            windows_final_path::same_drive(path).ok_or(err)
        }
        other => other,
    }
}

/// Handle-based final-path resolution for [`canonicalize`].
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 file-identity and final-path queries.
mod windows_final_path {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Component, Path, PathBuf, Prefix};

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, VOLUME_NAME_NONE,
    };

    /// Opens `path` (file or directory) without requesting data access.
    fn open(path: &Path) -> Option<std::fs::File> {
        std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)
            .ok()
    }

    /// Volume serial and file index: equal on both handles means one file.
    fn identity(file: &std::fs::File) -> Option<(u32, u32, u32)> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle is open for the duration of the call.
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.ok()?;
        Some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }

    /// `\\?\<drive>:` + the handle's volume-relative final path, if it is
    /// provably the same file as `path`.
    pub(super) fn same_drive(path: &Path) -> Option<PathBuf> {
        let absolute = std::path::absolute(path).ok()?;
        let drive = match absolute.components().next()? {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d,
                _ => return None,
            },
            _ => return None,
        };
        let file = open(&absolute)?;
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: `buf` is a live writable buffer and the handle is open.
            let len = unsafe {
                GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut buf, VOLUME_NAME_NONE)
            } as usize;
            if len == 0 {
                return None;
            }
            if len < buf.len() {
                buf.truncate(len);
                break;
            }
            buf.resize(len + 1, 0);
        }
        let rest = std::ffi::OsString::from_wide(&buf);
        let mut resolved = std::ffi::OsString::from(format!(r"\\?\{}:", char::from(drive)));
        resolved.push(rest);
        let resolved = PathBuf::from(resolved);
        let same = identity(&file)? == identity(&open(&resolved)?)?;
        same.then_some(resolved)
    }
}

/// True when `s` contains an interior NUL that would truncate a C argv/`CreateProcess` string.
fn os_contains_nul(s: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().contains(&0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().any(|c| c == 0)
    }
}

/// Rejects empty paths and interior NULs.
fn reject_empty_or_nul(path: &Path) -> Result<(), SpawnPathError> {
    if path.as_os_str().is_empty() {
        return Err(SpawnPathError::Empty);
    }
    if os_contains_nul(path.as_os_str()) {
        return Err(SpawnPathError::Nul);
    }
    Ok(())
}

/// True when `path` is a single normal component (PATH lookup), not a multi-segment relative path.
fn is_single_path_name(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Absolute path with no NUL, or a single PATH lookup name (`cargo`, `python3`).
///
/// Does not require the path to exist (useful for generated argv file operands).
/// Lexical `..` in an absolute spelling is allowed; callers that need a real
/// file should use [`require_spawn_executable`] (canonicalize).
///
/// Prefer not using this for operator-selected tools like `$CARGO` — those may
/// legitimately be relative (`./custom-cargo`). Use it when the product
/// contract is “absolute or PATH name” for a helper beside the host.
///
/// # Errors
///
/// Returns [`SpawnPathError`] when the path is empty, contains NUL, or is a
/// multi-component relative path.
pub fn require_absolute_or_name(path: &Path) -> Result<PathBuf, SpawnPathError> {
    reject_empty_or_nul(path)?;
    if path.is_absolute() || is_single_path_name(path) {
        return Ok(path.to_path_buf());
    }
    Err(SpawnPathError::NotAbsolute(path.to_path_buf()))
}

/// Like [`require_absolute_or_name`], but the path must be absolute (no PATH names).
///
/// # Errors
///
/// Returns [`SpawnPathError`] when the path is empty, contains NUL, or is not absolute.
pub fn require_absolute_spawn_path(path: &Path) -> Result<PathBuf, SpawnPathError> {
    reject_empty_or_nul(path)?;
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Err(SpawnPathError::NotAbsolute(path.to_path_buf()))
}

/// Absolute existing regular file (or a single PATH name), with no interior NUL.
///
/// Absolute paths are canonicalized so symlink targets and lexical `..` resolve
/// to the file that will actually be executed. PATH lookup names are returned
/// as-is for the OS to resolve at spawn time.
///
/// Prefer [`require_existing_regular_file`] for configured/env worker paths that
/// must identify a real file (including cwd-relative `./bin`) and must not
/// authorize ambient PATH lookup for a bare name.
///
/// # Errors
///
/// Returns [`SpawnPathError`] when validation fails, canonicalize fails, or an
/// absolute path is not a regular file.
pub fn require_spawn_executable(path: &Path) -> Result<PathBuf, SpawnPathError> {
    let path = require_absolute_or_name(path)?;
    if path.is_absolute() {
        let canon = canonicalize(&path).map_err(|source| SpawnPathError::Canonicalize {
            path: path.clone(),
            source,
        })?;
        if !canon.is_file() {
            return Err(SpawnPathError::NotFile(canon));
        }
        return Ok(canon);
    }
    Ok(path)
}

/// Existing regular file after resolving relative paths against the process cwd.
///
/// Absolute and multi-component relative paths (`./worker`, `bin/tool`) are
/// accepted when they canonicalize to a regular file. A bare name is accepted
/// only when that name exists as a file under the current directory — never as
/// an ambient `PATH` lookup. Use this for media worker / jail path knobs that
/// historically required `path.is_file()`.
///
/// # Errors
///
/// Returns [`SpawnPathError`] when the path is empty/NUL, cannot be resolved, or
/// is not a regular file.
pub fn require_existing_regular_file(path: &Path) -> Result<PathBuf, SpawnPathError> {
    reject_empty_or_nul(path)?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = std::env::current_dir().map_err(|source| SpawnPathError::Canonicalize {
            path: path.to_path_buf(),
            source,
        })?;
        cwd.join(path)
    };
    let canon = canonicalize(&absolute).map_err(|source| SpawnPathError::Canonicalize {
        path: absolute.clone(),
        source,
    })?;
    if !canon.is_file() {
        return Err(SpawnPathError::NotFile(canon));
    }
    Ok(canon)
}

/// Canonicalizes `path` and requires it to stay under canonical `root`.
///
/// When `path` does not exist yet, canonicalizes its parent and rejoins the
/// final component so generated files under a trusted session directory work.
///
/// # Errors
///
/// Returns [`SpawnPathError`] when the path is unsafe or escapes `root`.
pub fn require_under_root(path: &Path, root: &Path) -> Result<PathBuf, SpawnPathError> {
    let path = require_absolute_spawn_path(path)?;
    let root = require_absolute_spawn_path(root)?;
    let root_canon = canonicalize(&root).map_err(|source| SpawnPathError::Canonicalize {
        path: root.clone(),
        source,
    })?;
    let path_canon = match canonicalize(&path) {
        Ok(p) => p,
        Err(err) => {
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(SpawnPathError::Canonicalize {
                        path: path.clone(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "refusing dangling or unresolvable symlink",
                        ),
                    });
                }
                Ok(_) => {
                    return Err(SpawnPathError::Canonicalize {
                        path: path.clone(),
                        source: err,
                    });
                }
                Err(meta_err) if meta_err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(SpawnPathError::Canonicalize {
                        path: path.clone(),
                        source: meta_err,
                    });
                }
                Err(_) => {}
            }
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .ok_or_else(|| SpawnPathError::Canonicalize {
                    path: path.clone(),
                    source: err,
                })?;
            let parent_canon =
                canonicalize(parent).map_err(|source| SpawnPathError::Canonicalize {
                    path: parent.to_path_buf(),
                    source,
                })?;
            let name = path
                .file_name()
                .ok_or_else(|| SpawnPathError::NotAbsolute(path.clone()))?;
            parent_canon.join(name)
        }
    };
    if !path_canon.starts_with(&root_canon) {
        return Err(SpawnPathError::OutsideRoot {
            path: path_canon,
            root: root_canon,
        });
    }
    Ok(path_canon)
}

/// Accepts `path` when it equals `beside`'s sibling named `helper_name`, or is an
/// absolute existing file (env override).
///
/// Equality with the beside-host helper is the preferred product path. Absolute
/// env overrides still require [`require_spawn_executable`]; that is an
/// explicit operator trust decision, not proof the binary is safe.
///
/// Bare PATH names (single component, non-absolute) are rejected for this API
/// unless they match the beside-host helper. Callers that intentionally want
/// PATH lookup must use [`require_absolute_or_name`] / [`require_spawn_executable`]
/// directly.
///
/// # Errors
///
/// Returns [`SpawnPathError`] when validation fails.
pub fn require_helper_beside_or_absolute(
    path: &Path,
    helper_name: &str,
    beside: Option<&Path>,
) -> Result<PathBuf, SpawnPathError> {
    // Prefer beside match on the raw path before absolute-only enforcement so
    // `dir/bookclerk-workerd` next to the host still works.
    if let Some(beside) = beside {
        if let Some(dir) = beside.parent() {
            let expected = dir.join(helper_name);
            if path == expected {
                return require_spawn_executable(path);
            }
            if path.is_absolute() {
                if let (Ok(left), Ok(right)) = (canonicalize(path), canonicalize(&expected)) {
                    if left == right {
                        return require_spawn_executable(path);
                    }
                }
            }
        }
    }
    // Env override contract: absolute existing file only (no bare PATH names).
    let path = require_absolute_spawn_path(path)?;
    require_spawn_executable(&path)
}

/// Path passed to `CreateProcess`, `Command`, or a child that parses Win32 paths.
///
/// Windows canonical paths use the `\\?\` prefix. An AppContainer can open
/// that form for read and still be denied process creation
/// (`ERROR_ACCESS_DENIED`). Pinned `workerd` rejects the same prefix: its
/// filesystem parser treats `?` as the first component and aborts. The prefix
/// is removed only after callers have already canonicalized, so the Win32 path
/// names the same resolved file. Other platforms return `path` unchanged.
#[must_use]
pub fn create_process_path(path: &Path) -> PathBuf {
    strip_verbatim_prefix(path)
}

/// `\\?\` / `\\?\UNC\` → the Win32 path of an already-canonical file.
///
/// Non-Windows builds return `path` unchanged.
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_nul_and_relative_segments() {
        assert!(matches!(
            require_absolute_spawn_path(Path::new("a\0b")),
            Err(SpawnPathError::Nul)
        ));
        assert!(matches!(
            require_absolute_spawn_path(Path::new("rel/bin")),
            Err(SpawnPathError::NotAbsolute(_))
        ));
        assert!(require_absolute_or_name(Path::new("cargo")).is_ok());
    }

    #[test]
    fn spawn_executable_accepts_space_in_filename() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let bin = dir.path().join("my helper");
        std::fs::write(&bin, b"x").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).unwrap();
        }
        let got = require_spawn_executable(&bin).expect("space ok");
        assert_eq!(got, std::fs::canonicalize(&bin).unwrap());
    }

    #[test]
    fn under_root_accepts_child() {
        let root = tempfile::tempdir().expect("tmpdir");
        let child = root.path().join("workerd-config.capnp");
        std::fs::write(&child, b"x").expect("write");
        let got = require_under_root(&child, root.path()).expect("under root");
        assert_eq!(got, std::fs::canonicalize(&child).unwrap());
    }

    #[test]
    fn under_root_rejects_escape() {
        let root = tempfile::tempdir().expect("tmpdir");
        let other = tempfile::tempdir().expect("other");
        let escape = other.path().join("evil");
        std::fs::write(&escape, b"x").expect("write");
        assert!(matches!(
            require_under_root(&escape, root.path()),
            Err(SpawnPathError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn under_root_rejects_dangling_symlink_leaf() {
        let root = tempfile::tempdir().expect("tmpdir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("missing");
        let link = root.path().join("leaf");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        assert!(matches!(
            require_under_root(&link, root.path()),
            Err(SpawnPathError::Canonicalize { .. })
        ));
        assert!(!target.exists());
    }

    #[test]
    fn helper_beside_accepts_sibling_match() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let host = dir.path().join("bookclerkd");
        let helper = dir.path().join("bookclerk-workerd");
        std::fs::write(&host, b"host").expect("write");
        std::fs::write(&helper, b"helper").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&host, &helper] {
                let mut perms = std::fs::metadata(p).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(p, perms).unwrap();
            }
        }
        let got = require_helper_beside_or_absolute(&helper, "bookclerk-workerd", Some(&host))
            .expect("beside ok");
        assert_eq!(got, std::fs::canonicalize(&helper).unwrap());
    }

    #[test]
    fn helper_absolute_override_requires_existing_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let bin = dir.path().join("custom-workerd");
        std::fs::write(&bin, b"x").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).unwrap();
        }
        let got = require_helper_beside_or_absolute(&bin, "bookclerk-workerd", None)
            .expect("absolute file");
        assert_eq!(got, std::fs::canonicalize(&bin).unwrap());

        let missing = dir.path().join("missing-bin");
        assert!(matches!(
            require_helper_beside_or_absolute(&missing, "bookclerk-workerd", None),
            Err(SpawnPathError::Canonicalize { .. })
        ));

        assert!(matches!(
            require_helper_beside_or_absolute(dir.path(), "bookclerk-workerd", None),
            Err(SpawnPathError::NotFile(_)) | Err(SpawnPathError::Canonicalize { .. })
        ));
    }

    #[test]
    fn helper_rejects_bare_path_name_and_relative() {
        assert!(matches!(
            require_helper_beside_or_absolute(Path::new("workerd"), "bookclerk-workerd", None),
            Err(SpawnPathError::NotAbsolute(_))
        ));
        assert!(matches!(
            require_helper_beside_or_absolute(
                Path::new("rel/bookclerk-workerd"),
                "bookclerk-workerd",
                None
            ),
            Err(SpawnPathError::NotAbsolute(_))
        ));
        // Bare names remain valid for the general spawn APIs.
        assert!(require_absolute_or_name(Path::new("workerd")).is_ok());
    }

    #[test]
    fn existing_regular_file_resolves_relative_and_rejects_missing_bare_name() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let bin = dir.path().join("worker-bin");
        std::fs::write(&bin, b"x").expect("write");
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(dir.path()).expect("chdir");
        let got = require_existing_regular_file(Path::new("./worker-bin")).expect("./ ok");
        assert_eq!(got, std::fs::canonicalize(&bin).unwrap());
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let nested_bin = nested.join("tool");
        std::fs::write(&nested_bin, b"y").unwrap();
        let got = require_existing_regular_file(Path::new("nested/tool")).expect("nested ok");
        assert_eq!(got, std::fs::canonicalize(&nested_bin).unwrap());
        let bare = require_existing_regular_file(Path::new("worker-bin")).expect("cwd bare file");
        assert_eq!(bare, std::fs::canonicalize(&bin).unwrap());
        assert!(matches!(
            require_existing_regular_file(Path::new("not-on-disk-anywhere")),
            Err(SpawnPathError::Canonicalize { .. })
        ));
        assert!(matches!(
            require_existing_regular_file(Path::new("nested")),
            Err(SpawnPathError::NotFile(_))
        ));
        std::env::set_current_dir(prev).expect("restore cwd");
    }

    #[test]
    fn create_process_path_strips_verbatim_prefix() {
        let drive = Path::new(r"\\?\C:\work\workerd.exe");
        let unc = Path::new(r"\\?\UNC\server\share\workerd.exe");
        let plain = Path::new(r"C:\work\workerd.exe");
        #[cfg(windows)]
        {
            assert_eq!(
                create_process_path(drive),
                PathBuf::from(r"C:\work\workerd.exe")
            );
            assert_eq!(
                create_process_path(unc),
                PathBuf::from(r"\\server\share\workerd.exe")
            );
            assert_eq!(create_process_path(plain), plain);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(create_process_path(drive), drive);
            assert_eq!(create_process_path(unc), unc);
            assert_eq!(create_process_path(plain), plain);
        }
    }
}
