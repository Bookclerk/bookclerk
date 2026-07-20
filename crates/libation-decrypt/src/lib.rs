//! Decrypt pipeline: aaxclean-cli (Adrm + CENC) and optional ffmpeg mp3 encode.

mod error;
mod metadata;

pub use error::{DecryptError, Result};
pub use metadata::{fixup_audiobook, FixupRequest};

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

/// Input for an Adrm aaxc decrypt job.
#[derive(Debug, Clone)]
pub struct DecryptRequest {
    /// Encrypted AAX/AAXC file.
    pub input: PathBuf,
    /// Destination m4b/m4a path.
    pub output: PathBuf,
    /// Adrm aaxc content key (hex) — preferred modern path.
    pub audible_key: Option<String>,
    /// Adrm aaxc IV (hex).
    pub audible_iv: Option<String>,
    /// Legacy AAX activation bytes (unsupported by aaxclean-cli key/iv path).
    pub activation_bytes: Option<String>,
    /// Path to `aaxclean-cli` binary (default: `AUDIBLE_AAXCLEAN_CLI` or `PATH`).
    pub aaxclean_bin: Option<PathBuf>,
}

/// Input for a Widevine CENC decrypt job.
#[derive(Debug, Clone)]
pub struct CencDecryptRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    /// 32-hex key id.
    pub kid: String,
    /// 32-hex content key.
    pub key: String,
    pub aaxclean_bin: Option<PathBuf>,
    pub ffmpeg_bin: Option<PathBuf>,
}

/// Outcome of a successful decrypt / encode.
#[derive(Debug, Clone)]
pub struct DecryptOutcome {
    pub output: PathBuf,
}

/// Build argv for aaxclean-cli Adrm decrypt (without the binary).
pub fn aaxclean_args(req: &DecryptRequest) -> Result<Vec<String>> {
    match (&req.audible_key, &req.audible_iv) {
        (Some(key), Some(iv)) => Ok(vec![
            "-f".into(),
            req.input.display().to_string(),
            "--audible_key".into(),
            key.clone(),
            "--audible_iv".into(),
            iv.clone(),
            "--moov_faststart".into(),
            "-o".into(),
            req.output.display().to_string(),
        ]),
        _ if req.activation_bytes.is_some() => Err(DecryptError::UnsupportedActivationBytes),
        _ => Err(DecryptError::MissingCredentials),
    }
}

/// Build argv for aaxclean-cli CENC decrypt.
pub fn aaxclean_cenc_args(req: &CencDecryptRequest) -> Vec<String> {
    vec![
        "-f".into(),
        req.input.display().to_string(),
        "--encryption_kid".into(),
        req.kid.clone(),
        "--encryption_key".into(),
        req.key.clone(),
        "--moov_faststart".into(),
        "-o".into(),
        req.output.display().to_string(),
    ]
}

fn resolve_aaxclean_bin(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Ok(path) = std::env::var("AUDIBLE_AAXCLEAN_CLI") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from("aaxclean-cli")
}

fn resolve_ffmpeg_bin(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Ok(path) = std::env::var("LIBATION_FFMPEG") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from("ffmpeg")
}

/// Decrypt Adrm aaxc using `aaxclean-cli`.
pub async fn decrypt_with_aaxclean(req: DecryptRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DecryptError::InputMissing(req.input));
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let args = aaxclean_args(&req)?;
    let bin = resolve_aaxclean_bin(req.aaxclean_bin.as_deref());
    run_tool(&bin, &args, &req.output, "aaxclean-cli").await?;
    Ok(DecryptOutcome { output: req.output })
}

/// Decrypt Widevine CENC using aaxclean-cli, falling back to ffmpeg `-decryption_key`.
pub async fn decrypt_cenc(req: CencDecryptRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DecryptError::InputMissing(req.input.clone()));
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let aax_bin = resolve_aaxclean_bin(req.aaxclean_bin.as_deref());
    if tool_available(&aax_bin).await {
        let args = aaxclean_cenc_args(&req);
        match run_tool(&aax_bin, &args, &req.output, "aaxclean-cli").await {
            Ok(()) => return Ok(DecryptOutcome { output: req.output }),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "aaxclean-cli CENC decrypt failed; trying ffmpeg"
                );
            }
        }
    }

    let ffmpeg = resolve_ffmpeg_bin(req.ffmpeg_bin.as_deref());
    if !tool_available(&ffmpeg).await {
        return Err(DecryptError::DecryptToolMissing {
            aaxclean: aax_bin,
            ffmpeg,
        });
    }

    // Never log the key — only paths and binary name.
    tracing::info!(
        input = %req.input.display(),
        output = %req.output.display(),
        bin = %ffmpeg.display(),
        "running ffmpeg CENC decrypt"
    );
    let output = Command::new(&ffmpeg)
        .args([
            "-y",
            "-nostdin",
            "-loglevel",
            "error",
            "-decryption_key",
            &req.key,
            "-i",
            &req.input.display().to_string(),
            "-c",
            "copy",
            "-map_metadata",
            "0",
            "-movflags",
            "+faststart",
            &req.output.display().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(DecryptError::Io)?;

    if !output.status.success() {
        let _ = tokio::fs::remove_file(&req.output).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DecryptError::FfmpegFailed {
            status: output.status.code(),
            stderr: stderr.trim().to_string(),
        });
    }
    if !req.output.exists() {
        return Err(DecryptError::OutputMissing(req.output.clone()));
    }
    Ok(DecryptOutcome { output: req.output })
}

/// Re-encode audio to MP3 via ffmpeg (classic Libation `DecryptToLossy`).
pub async fn encode_to_mp3(
    input: &Path,
    output: &Path,
    ffmpeg_bin: Option<&Path>,
    lame: &libation_config::LameConfig,
    max_sample_rate: Option<u32>,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DecryptError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let ffmpeg = resolve_ffmpeg_bin(ffmpeg_bin);
    if !tool_available(&ffmpeg).await {
        return Err(DecryptError::FfmpegNotFound(ffmpeg));
    }

    tracing::info!(
        input = %input.display(),
        output = %output.display(),
        bin = %ffmpeg.display(),
        "running ffmpeg mp3 encode"
    );
    let mut args = vec![
        "-y".to_string(),
        "-nostdin".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input.display().to_string(),
        "-codec:a".to_string(),
        "libmp3lame".to_string(),
    ];
    if lame.constant_bitrate || lame.target.eq_ignore_ascii_case("bitrate") {
        args.push("-b:a".into());
        args.push(format!("{}k", lame.bitrate_kbps));
    } else {
        args.push("-qscale:a".into());
        args.push(lame.vbr_quality.to_string());
    }
    if lame.downsample_mono || lame.mode.eq_ignore_ascii_case("mono") {
        args.push("-ac".into());
        args.push("1".into());
    }
    if let Some(max_hz) = max_sample_rate {
        args.push("-ar".into());
        args.push(max_hz.to_string());
    }
    args.push("-map_metadata".into());
    args.push("0".into());
    args.push("-id3v2_version".into());
    args.push("3".into());
    args.push(output.display().to_string());

    let output_status = Command::new(&ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(DecryptError::Io)?;

    if !output_status.status.success() {
        let _ = tokio::fs::remove_file(output).await;
        let stderr = String::from_utf8_lossy(&output_status.stderr);
        return Err(DecryptError::FfmpegFailed {
            status: output_status.status.code(),
            stderr: stderr.trim().to_string(),
        });
    }
    if !output.exists() {
        return Err(DecryptError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}

async fn run_tool(bin: &Path, args: &[String], output: &Path, label: &str) -> Result<()> {
    tracing::info!(
        output = %output.display(),
        bin = %bin.display(),
        "{label}"
    );
    let result = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                DecryptError::AaxcleanNotFound(bin.to_path_buf())
            } else {
                DecryptError::Io(err)
            }
        })?;

    if !result.status.success() {
        let _ = tokio::fs::remove_file(output).await;
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(DecryptError::AaxcleanFailed {
            status: result.status.code(),
            stderr: stderr.trim().to_string(),
        });
    }
    if !output.exists() {
        return Err(DecryptError::OutputMissing(output.to_path_buf()));
    }
    Ok(())
}

async fn tool_available(bin: &Path) -> bool {
    Command::new(bin)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
        || Command::new(bin)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
}

/// True when `aaxclean-cli` appears to be available.
pub async fn aaxclean_available(bin: Option<&Path>) -> bool {
    tool_available(&resolve_aaxclean_bin(bin)).await
}

/// True when `ffmpeg` appears to be available.
pub async fn ffmpeg_available(bin: Option<&Path>) -> bool {
    tool_available(&resolve_ffmpeg_bin(bin)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aaxclean_args_use_key_iv_shape() {
        let req = DecryptRequest {
            input: PathBuf::from("/cache/book.aaxc"),
            output: PathBuf::from("/cache/book.m4b"),
            audible_key: Some("aabb".into()),
            audible_iv: Some("ccdd".into()),
            activation_bytes: None,
            aaxclean_bin: None,
        };
        let args = aaxclean_args(&req).unwrap();
        assert_eq!(
            args,
            vec![
                "-f",
                "/cache/book.aaxc",
                "--audible_key",
                "aabb",
                "--audible_iv",
                "ccdd",
                "--moov_faststart",
                "-o",
                "/cache/book.m4b",
            ]
        );
    }

    #[test]
    fn aaxclean_cenc_args_shape() {
        let req = CencDecryptRequest {
            input: PathBuf::from("in.cenc"),
            output: PathBuf::from("out.m4b"),
            kid: "aa".repeat(16),
            key: "bb".repeat(16),
            aaxclean_bin: None,
            ffmpeg_bin: None,
        };
        let args = aaxclean_cenc_args(&req);
        assert!(args.contains(&"--encryption_kid".into()));
        assert!(args.contains(&"--encryption_key".into()));
    }

    #[test]
    fn activation_bytes_alone_rejected() {
        let req = DecryptRequest {
            input: PathBuf::from("in.aax"),
            output: PathBuf::from("out.m4b"),
            audible_key: None,
            audible_iv: None,
            activation_bytes: Some("deadbeef".into()),
            aaxclean_bin: None,
        };
        assert!(matches!(
            aaxclean_args(&req),
            Err(DecryptError::UnsupportedActivationBytes)
        ));
    }
}
