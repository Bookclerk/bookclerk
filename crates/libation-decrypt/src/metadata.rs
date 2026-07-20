//! Embed tags, cover art, and chapters via ffmpeg (classic Libation fix-up).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{DecryptError, Result};
use crate::{ffmpeg_available, DecryptOutcome};

/// Request to fix up audiobook metadata after decrypt.
#[derive(Debug, Clone)]
pub struct FixupRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    pub title: String,
    pub author: Option<String>,
    pub narrator: Option<String>,
    pub cover: Option<PathBuf>,
    pub ffmetadata: Option<PathBuf>,
    pub ffmpeg_bin: Option<PathBuf>,
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

/// Apply metadata tags, optional cover embed, and optional chapters.
pub async fn fixup_audiobook(req: FixupRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DecryptError::InputMissing(req.input));
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let ffmpeg = resolve_ffmpeg_bin(req.ffmpeg_bin.as_deref());
    if !ffmpeg_available(req.ffmpeg_bin.as_deref()).await {
        return Err(DecryptError::FfmpegNotFound(ffmpeg));
    }

    let mut cmd = Command::new(&ffmpeg);
    cmd.args(["-y", "-nostdin", "-loglevel", "error", "-i"])
        .arg(&req.input);

    let mut map_chapters = false;
    let mut next_input = 1usize;
    let meta_idx = if let Some(meta) = &req.ffmetadata {
        cmd.args(["-i"]).arg(meta);
        map_chapters = true;
        let idx = next_input;
        next_input += 1;
        Some(idx)
    } else {
        None
    };
    let cover_idx = if let Some(cover) = &req.cover {
        cmd.args(["-i"]).arg(cover);
        Some(next_input)
    } else {
        None
    };

    cmd.args(["-map", "0:a?"]);
    if let Some(ci) = cover_idx {
        cmd.args(["-map", &format!("{ci}:v:0")]);
    }

    if map_chapters {
        if let Some(mi) = meta_idx {
            cmd.args([
                "-map_metadata",
                &mi.to_string(),
                "-map_chapters",
                &mi.to_string(),
            ]);
        }
    }

    cmd.args(["-c", "copy"]);

    if let Some(author) = req.author.as_deref().filter(|s| !s.is_empty()) {
        cmd.args(["-metadata", &format!("artist={author}")]);
        cmd.args(["-metadata", &format!("album_artist={author}")]);
    }
    cmd.args(["-metadata", &format!("title={}", req.title)]);
    cmd.args(["-metadata", &format!("album={}", req.title)]);
    if let Some(narrator) = req.narrator.as_deref().filter(|s| !s.is_empty()) {
        cmd.args(["-metadata", &format!("composer={narrator}")]);
    }
    if req.cover.is_some() {
        cmd.args(["-disposition:v:0", "attached_pic"]);
    }

    let ext = req
        .output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext.eq_ignore_ascii_case("m4b") || ext.eq_ignore_ascii_case("m4a") {
        cmd.args(["-movflags", "+faststart"]);
    }

    cmd.arg(&req.output);

    tracing::info!(
        input = %req.input.display(),
        output = %req.output.display(),
        cover = req.cover.is_some(),
        chapters = req.ffmetadata.is_some(),
        "running ffmpeg metadata fixup"
    );

    let output = cmd
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
        return Err(DecryptError::OutputMissing(req.output));
    }
    Ok(DecryptOutcome { output: req.output })
}
