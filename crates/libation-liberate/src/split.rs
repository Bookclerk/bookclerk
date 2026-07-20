//! Split liberated audio by chapter (classic `SplitFilesByChapter`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use libation_audible::DownloadOptions;
use libation_decrypt::ffmpeg_available;
use tokio::process::Command;

use crate::cue::FlatChapter;
use crate::error::{LiberateError, Result};
use crate::naming::{chapter_storage_key_with_folder, NamingContext};

/// One output chapter file after splitting.
#[derive(Debug, Clone)]
pub struct SplitChapterFile {
    pub path: PathBuf,
    pub storage_key: String,
    pub title: String,
}

/// Split `input` into per-chapter files using ffmpeg stream copy.
#[allow(clippy::too_many_arguments)]
pub async fn split_audio_by_chapters(
    input: &Path,
    output_dir: &Path,
    chapters: &[FlatChapter],
    total_duration_ms: u64,
    folder_ctx: &NamingContext,
    file_ctx: &NamingContext,
    options: &DownloadOptions,
    ext: &str,
    ffmpeg_bin: Option<&Path>,
) -> Result<Vec<SplitChapterFile>> {
    if chapters.is_empty() {
        return Err(LiberateError::Other(anyhow::anyhow!(
            "split-by-chapter requested but no chapters available"
        )));
    }
    let ffmpeg = ffmpeg_bin
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    if !ffmpeg_available(ffmpeg_bin).await {
        return Err(LiberateError::Decrypt(
            libation_decrypt::DecryptError::FfmpegNotFound(ffmpeg),
        ));
    }
    tokio::fs::create_dir_all(output_dir).await?;

    let groups = group_chapters(chapters, total_duration_ms, options);
    let mut outputs = Vec::new();
    for (idx, group) in groups.iter().enumerate() {
        let start_ms = group.first().map(|c| c.start_ms).unwrap_or(0);
        let end_ms = group
            .last()
            .and_then(|last| {
                chapters
                    .iter()
                    .find(|c| c.start_ms > last.start_ms)
                    .map(|c| c.start_ms)
            })
            .unwrap_or(total_duration_ms);
        let title = group
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>()
            .join(": ");
        let chapter_no = idx + 1;
        let filename = chapter_storage_key_with_folder(
            folder_ctx,
            file_ctx,
            options.folder_template.as_deref(),
            options.chapter_file_template.as_deref(),
            &options.replacement_characters,
            chapter_no,
            &title,
            ext,
        );
        let out_path = output_dir.join(filename.rsplit('/').next().unwrap_or(&filename));
        let start = format_duration(start_ms);
        let duration = format_duration(end_ms.saturating_sub(start_ms));
        let status = Command::new(&ffmpeg)
            .args([
                "-y",
                "-nostdin",
                "-loglevel",
                "error",
                "-ss",
                &start,
                "-i",
                &input.display().to_string(),
                "-t",
                &duration,
                "-c",
                "copy",
                &out_path.display().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| LiberateError::Other(anyhow::anyhow!("ffmpeg split: {e}")))?;
        if !status.status.success() {
            let stderr = String::from_utf8_lossy(&status.stderr);
            return Err(LiberateError::Other(anyhow::anyhow!(
                "ffmpeg chapter split failed: {}",
                stderr.trim()
            )));
        }
        outputs.push(SplitChapterFile {
            path: out_path,
            storage_key: filename,
            title,
        });
    }
    Ok(outputs)
}

fn group_chapters(
    chapters: &[FlatChapter],
    total_duration_ms: u64,
    options: &DownloadOptions,
) -> Vec<Vec<FlatChapter>> {
    let min_ms = u64::from(options.minimum_file_duration_minutes) * 60_000;
    if min_ms == 0 || chapters.len() <= 1 {
        return chapters.iter().map(|c| vec![c.clone()]).collect();
    }
    let mut groups: Vec<Vec<FlatChapter>> = Vec::new();
    let mut current: Vec<FlatChapter> = Vec::new();
    let mut current_start = 0u64;
    for (idx, ch) in chapters.iter().enumerate() {
        current.push(ch.clone());
        let next_start = chapters
            .get(idx + 1)
            .map(|c| c.start_ms)
            .unwrap_or(total_duration_ms);
        let duration = next_start.saturating_sub(current_start);
        if duration >= min_ms {
            groups.push(std::mem::take(&mut current));
            current_start = next_start;
        }
    }
    if !current.is_empty() {
        if let Some(last) = groups.last_mut() {
            last.extend(current);
        } else {
            groups.push(current);
        }
    }
    groups
}

fn format_duration(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    format!("{secs:.3}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_short_chapters_when_minimum_set() {
        let chapters = vec![
            FlatChapter {
                title: "A".into(),
                start_ms: 0,
            },
            FlatChapter {
                title: "B".into(),
                start_ms: 30_000,
            },
            FlatChapter {
                title: "C".into(),
                start_ms: 60_000,
            },
        ];
        let opts = DownloadOptions {
            minimum_file_duration_minutes: 2,
            ..Default::default()
        };
        let groups = group_chapters(&chapters, 120_000, &opts);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }
}
