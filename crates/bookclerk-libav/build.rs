//! Builds the pinned FFmpeg release as static, decoders-only libraries and
//! compiles the C shim against it.
//!
//! The source is the release tarball named by [`FFMPEG_VERSION`], fetched
//! once into `OUT_DIR` and checked against [`FFMPEG_SHA256`] before it is
//! unpacked (the digest was taken from a download whose release signature
//! verified against FFmpeg's signing key). `configure` starts from
//! `--disable-everything` and turns on only the decoders, demuxers and
//! parser the media worker uses, with no programs, protocols, devices,
//! filters, encoders, hardware acceleration, threads or autodetected
//! libraries, and without the GPL or nonfree parts, so the libraries stay
//! LGPL-2.1-or-later.
//!
//! The build needs `curl`, `tar` (with xz), a POSIX shell and `make` on the
//! host, which every Linux and macOS builder has. On Windows the target
//! must be MSVC: FFmpeg's `configure` runs with `--toolchain=msvc` under
//! MSYS2's shell and `make` (found at `MSYS2_ROOT`, default `C:\msys64`;
//! `pacman -S make` adds `make`), with the MSVC compiler the `cc` crate
//! finds. Other targets get an empty library.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// The FFmpeg release built.
const FFMPEG_VERSION: &str = "9.0.2";

/// SHA-256 of `ffmpeg-<FFMPEG_VERSION>.tar.xz`, lowercase hex.
const FFMPEG_SHA256: &str = "8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e";

/// Where the release tarball is downloaded from.
const FFMPEG_URL_BASE: &str = "https://ffmpeg.org/releases";

/// Decoders built: AC-3, E-AC-3, the four WMA codecs, AMR-NB and AMR-WB.
const DECODERS: &str = "ac3,eac3,wmav1,wmav2,wmapro,wmalossless,amrnb,amrwb";

/// Demuxers built: ASF (WMA files), raw AC-3/E-AC-3, raw AMR and
/// ISO-BMFF (3GP files carrying AMR).
const DEMUXERS: &str = "asf,ac3,eac3,amr,amrnb,amrwb,mov";

/// Parsers built: AC-3/E-AC-3 framing for the raw streams.
const PARSERS: &str = "ac3";

/// Builds FFmpeg (once per `OUT_DIR`), compiles the shim and emits the
/// link lines.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-env-changed=MSYS2_ROOT");
    let host = Host::for_target();
    if host == Host::Unsupported {
        // The library is empty there (the `cfg` in lib.rs), so a workspace
        // build still compiles it without FFmpeg.
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default());
    let prefix = out.join("ffmpeg-install");
    let stamp = prefix.join(format!(".built-{FFMPEG_VERSION}"));
    if !stamp.exists() {
        let source = fetch_source(host, &out);
        build_ffmpeg(host, &out, &source, &prefix);
        fs::write(&stamp, FFMPEG_VERSION).unwrap_or_else(|err| panic!("write stamp: {err}"));
        // Only the installed libraries and headers are needed from here
        // on; the source, objects and tarball are about 150 MB.
        for leftover in [source, out.join("ffmpeg-build")] {
            let _ = fs::remove_dir_all(leftover);
        }
        let _ = fs::remove_file(out.join(format!("ffmpeg-{FFMPEG_VERSION}.tar.xz")));
    }
    let include = prefix.join("include");
    cc::Build::new()
        .file("src/shim.c")
        .include(&include)
        .warnings(true)
        .compile("bookclerk_libav_shim");
    let lib = prefix.join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    // Order matters for static linking: users before what they use.
    for name in ["avformat", "avcodec", "avutil"] {
        if host == Host::Msvc {
            msvc_library_name(&lib, name);
        }
        println!("cargo:rustc-link-lib=static={name}");
    }
    match host {
        Host::Unix => println!("cargo:rustc-link-lib=m"),
        // libavutil seeds its random numbers from BCryptGenRandom.
        Host::Msvc => println!("cargo:rustc-link-lib=bcrypt"),
        Host::Unsupported => {}
    }
}

/// How the target's FFmpeg is built.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Host {
    /// Linux, macOS and the other Unix targets: the host's shell, `make`
    /// and C compiler.
    Unix,
    /// Windows with the MSVC toolchain: MSYS2's shell and `make` driving
    /// `cl.exe`.
    Msvc,
    /// Anything else: no FFmpeg, an empty library.
    Unsupported,
}

impl Host {
    /// Picks the build for the target Cargo is compiling for.
    fn for_target() -> Self {
        if env::var_os("CARGO_CFG_UNIX").is_some() {
            return Self::Unix;
        }
        let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
        let msvc = env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
        if windows && msvc {
            Self::Msvc
        } else {
            Self::Unsupported
        }
    }
}

/// MSYS2's `usr\bin`, which holds the `sh`, `make` and `tar` the Windows
/// build runs.
///
/// # Panics
///
/// When MSYS2 is not installed at `MSYS2_ROOT` (default `C:\msys64`).
fn msys2_bin() -> PathBuf {
    let root = env::var_os("MSYS2_ROOT").map_or_else(|| PathBuf::from(r"C:\msys64"), PathBuf::from);
    let bin = root.join("usr").join("bin");
    if !bin.join("sh.exe").exists() {
        panic!(
            "building FFmpeg for MSVC needs MSYS2 (with `pacman -S make`); none at {}; set MSYS2_ROOT",
            root.display()
        );
    }
    bin
}

/// A Windows path as MSYS2's tools read it: forward slashes, drive kept
/// (`C:/x/y`).
fn msys_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// A command run with MSYS2's tools, then the MSVC compiler's environment
/// (`PATH`, `INCLUDE`, `LIB`) from the `cc` crate, on `PATH`.
///
/// MSYS2's directory comes first so `configure` and the recipes find its
/// `make`, `sed` and `awk` rather than native ports elsewhere on `PATH`
/// (a native `make` can't read MSYS paths). MSYS2's own `link.exe` then
/// shadows MSVC's, which FFmpeg's MSVC toolchain expects: it links
/// through `compat/windows/mslink`, which skips it.
fn msys_command(program: &str) -> Command {
    let bin = msys2_bin();
    let compiler = cc::Build::new().get_compiler();
    let mut cmd = Command::new(bin.join(format!("{program}.exe")));
    // The plain MSYS environment (not MinGW): POSIX tools, no compiler of
    // its own.
    cmd.env("MSYSTEM", "MSYS");
    let mut path = vec![bin];
    if let Some(compiler_dir) = compiler.path().parent() {
        path.push(compiler_dir.to_path_buf());
    }
    let mut compiler_path = None;
    for (key, value) in compiler.env() {
        if key.eq_ignore_ascii_case("PATH") {
            compiler_path = Some(value.clone());
        } else {
            cmd.env(key, value);
        }
    }
    // The compiler's `PATH` already ends with this process's.
    let rest = compiler_path.or_else(|| env::var_os("PATH"));
    path.extend(rest.iter().flat_map(env::split_paths));
    cmd.env(
        "PATH",
        env::join_paths(path).unwrap_or_else(|err| panic!("PATH: {err}")),
    );
    cmd
}

/// Makes `<name>.lib` exist in `lib`, the name Rust links on MSVC.
///
/// FFmpeg's MSVC build names its static libraries `lib<name>.a`; the
/// archive format is already MSVC's, so a copy under the other name is
/// enough.
///
/// # Panics
///
/// When neither file exists or the copy fails.
fn msvc_library_name(lib: &Path, name: &str) {
    let want = lib.join(format!("{name}.lib"));
    if want.exists() {
        return;
    }
    let made = lib.join(format!("lib{name}.a"));
    fs::copy(&made, &want).unwrap_or_else(|err| panic!("copy {}: {err}", made.display()));
}

/// Downloads and unpacks the pinned tarball into `out`, returning the
/// source directory.
fn fetch_source(host: Host, out: &Path) -> PathBuf {
    let name = format!("ffmpeg-{FFMPEG_VERSION}");
    let tarball = out.join(format!("{name}.tar.xz"));
    if !tarball.exists() || digest(&tarball) != FFMPEG_SHA256 {
        let partial = out.join(format!("{name}.tar.xz.part"));
        let url = format!("{FFMPEG_URL_BASE}/{name}.tar.xz");
        run(Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--retry", "4", "--retry-delay", "2", "--output"])
            .arg(&partial)
            .arg(&url));
        let got = digest(&partial);
        if got != FFMPEG_SHA256 {
            panic!("{url} has SHA-256 {got}, expected {FFMPEG_SHA256}");
        }
        fs::rename(&partial, &tarball).unwrap_or_else(|err| panic!("keep tarball: {err}"));
    }
    let source = out.join(&name);
    if source.exists() {
        fs::remove_dir_all(&source).unwrap_or_else(|err| panic!("clear source: {err}"));
    }
    if host == Host::Msvc {
        // MSYS2's tar, which has xz; `--force-local` keeps it from reading
        // `C:` as a remote host.
        run(msys_command("tar")
            .args(["--force-local", "-xJf"])
            .arg(msys_path(&tarball))
            .arg("-C")
            .arg(msys_path(out)));
    } else {
        run(Command::new("tar")
            .arg("-xJf")
            .arg(&tarball)
            .arg("-C")
            .arg(out));
    }
    source
}

/// Configures, builds and installs the libraries from `source` under
/// `prefix`, building in `out/ffmpeg-build`.
fn build_ffmpeg(host: Host, out: &Path, source: &Path, prefix: &Path) {
    let build = out.join("ffmpeg-build");
    if build.exists() {
        fs::remove_dir_all(&build).unwrap_or_else(|err| panic!("clear build dir: {err}"));
    }
    fs::create_dir_all(&build).unwrap_or_else(|err| panic!("create build dir: {err}"));
    let msvc = host == Host::Msvc;
    let shell_path = |path: &Path| {
        if msvc {
            msys_path(path)
        } else {
            path.display().to_string()
        }
    };
    let mut configure = if msvc {
        msys_command("sh")
    } else {
        Command::new("sh")
    };
    configure
        .current_dir(&build)
        .arg(shell_path(&source.join("configure")))
        .arg(format!("--prefix={}", shell_path(prefix)))
        .args([
            "--disable-everything",
            "--disable-programs",
            "--disable-doc",
            "--disable-avdevice",
            "--disable-avfilter",
            "--disable-swscale",
            "--disable-swresample",
            "--disable-network",
            "--disable-protocols",
            "--disable-autodetect",
            "--disable-pthreads",
            "--disable-w32threads",
            "--disable-x86asm",
            "--disable-debug",
            "--enable-static",
            "--disable-shared",
            "--enable-pic",
        ])
        .arg(format!("--enable-decoder={DECODERS}"))
        .arg(format!("--enable-demuxer={DEMUXERS}"))
        .arg(format!("--enable-parser={PARSERS}"));
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if msvc {
        // `cl.exe` from the `cc` crate's environment is already the
        // target's compiler; the C runtime matches Rust's (`/MD` unless
        // the target links it statically).
        let static_crt = env::var("CARGO_CFG_TARGET_FEATURE")
            .is_ok_and(|features| features.split(',').any(|f| f == "crt-static"));
        let crt = if static_crt { "-MT" } else { "-MD" };
        configure
            .arg("--toolchain=msvc")
            .arg(format!("--arch={arch}"))
            .arg("--target-os=win32")
            .arg(format!("--extra-cflags={crt}"));
        if env::var("HOST").unwrap_or_default() != env::var("TARGET").unwrap_or_default() {
            configure.arg("--enable-cross-compile");
        }
    } else {
        let compiler = cc::Build::new().get_compiler();
        configure.arg(format!("--cc={}", compiler.path().display()));
        let host = env::var("HOST").unwrap_or_default();
        let target = env::var("TARGET").unwrap_or_default();
        if host != target {
            let os = match env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
                "macos" => "darwin".to_string(),
                other => other.to_string(),
            };
            configure
                .arg("--enable-cross-compile")
                .arg(format!("--arch={arch}"))
                .arg(format!("--target-os={os}"));
        }
    }
    run(without_host_flags(&mut configure));
    let jobs = env::var("NUM_JOBS").unwrap_or_else(|_| "1".to_string());
    for target in [format!("-j{jobs}"), "install".to_string()] {
        let mut make = if msvc {
            // Through MSYS2's shell, not straight from this process: a
            // `make` started by a native program runs FFmpeg's recipes
            // with one level of backslashes lost (its `awk` and `sed`
            // dependency scripts then fail to parse).
            let mut sh = msys_command("sh");
            sh.arg("-c").arg(format!("make {target}"));
            sh
        } else {
            let mut make = Command::new("make");
            make.arg(target);
            make
        };
        run(without_host_flags(make.current_dir(&build)));
    }
}

/// Lowercase hex SHA-256 of the file at `path`.
fn digest(path: &Path) -> String {
    let mut file =
        fs::File::open(path).unwrap_or_else(|err| panic!("open {}: {err}", path.display()));
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let read = file
            .read(&mut buf)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Clears the C flags a fuzzing or sanitizer build exports for its own
/// code (ClusterFuzzLite sets `-fsanitize=fuzzer-no-link` and the like),
/// so FFmpeg builds plainly. FFmpeg itself is not instrumented; the shim,
/// which `cc` compiles, keeps those flags.
fn without_host_flags(cmd: &mut Command) -> &mut Command {
    for name in ["CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS"] {
        cmd.env_remove(name);
    }
    cmd
}

/// Runs `cmd`, panicking with its command line when it fails.
fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|err| panic!("spawn {cmd:?}: {err}"));
    if !status.success() {
        panic!("{cmd:?} exited with {status}");
    }
}
