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
//! host, which every Linux and macOS builder has. Off Unix the script does
//! nothing and the library is empty; the feature that pulls this crate in
//! stays off there.

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
    if env::var_os("CARGO_CFG_UNIX").is_none() {
        // The library is empty off Unix (`#![cfg(unix)]` in lib.rs), so a
        // workspace build on Windows compiles it without FFmpeg.
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default());
    let prefix = out.join("ffmpeg-install");
    let stamp = prefix.join(format!(".built-{FFMPEG_VERSION}"));
    if !stamp.exists() {
        let source = fetch_source(&out);
        build_ffmpeg(&out, &source, &prefix);
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
        println!("cargo:rustc-link-lib=static={name}");
    }
    println!("cargo:rustc-link-lib=m");
}

/// Downloads and unpacks the pinned tarball into `out`, returning the
/// source directory.
fn fetch_source(out: &Path) -> PathBuf {
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
    run(Command::new("tar")
        .arg("-xJf")
        .arg(&tarball)
        .arg("-C")
        .arg(out));
    source
}

/// Configures, builds and installs the libraries from `source` under
/// `prefix`, building in `out/ffmpeg-build`.
fn build_ffmpeg(out: &Path, source: &Path, prefix: &Path) {
    let build = out.join("ffmpeg-build");
    if build.exists() {
        fs::remove_dir_all(&build).unwrap_or_else(|err| panic!("clear build dir: {err}"));
    }
    fs::create_dir_all(&build).unwrap_or_else(|err| panic!("create build dir: {err}"));
    let compiler = cc::Build::new().get_compiler();
    let mut configure = Command::new("sh");
    configure
        .current_dir(&build)
        .arg(source.join("configure"))
        .arg(format!("--prefix={}", prefix.display()))
        .arg(format!("--cc={}", compiler.path().display()))
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
            "--disable-x86asm",
            "--disable-debug",
            "--enable-static",
            "--disable-shared",
            "--enable-pic",
        ])
        .arg(format!("--enable-decoder={DECODERS}"))
        .arg(format!("--enable-demuxer={DEMUXERS}"))
        .arg(format!("--enable-parser={PARSERS}"));
    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    if host != target {
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let os = match env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
            "macos" => "darwin".to_string(),
            other => other.to_string(),
        };
        configure
            .arg("--enable-cross-compile")
            .arg(format!("--arch={arch}"))
            .arg(format!("--target-os={os}"));
    }
    run(&mut configure);
    let jobs = env::var("NUM_JOBS").unwrap_or_else(|_| "1".to_string());
    run(Command::new("make")
        .current_dir(&build)
        .arg(format!("-j{jobs}")));
    run(Command::new("make").current_dir(&build).arg("install"));
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

/// Runs `cmd`, panicking with its command line when it fails.
fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|err| panic!("spawn {cmd:?}: {err}"));
    if !status.success() {
        panic!("{cmd:?} exited with {status}");
    }
}
