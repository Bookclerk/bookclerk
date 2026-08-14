//! Split acquired audio by chapter (classic `SplitFilesByChapter`).

use std::path::{Path, PathBuf};

use bookclerk_media::{remux_trimmed_async, TrimRange};
use bookclerk_source::DownloadOptions;

use crate::cue::FlatChapter;
use crate::error::{AcquireError, Result};
use crate::naming::{chapter_storage_key_with_folder, NamingContext};

/// One output chapter file after splitting.
#[derive(Debug, Clone)]
pub struct SplitChapterFile {
    /// Filesystem path of the remuxed chapter file under `output_dir`.
    pub path: PathBuf,
    /// Chapter title(s) joined with `: ` when several short chapters were grouped.
    pub title: String,
}

/// Split `input` into per-chapter files using native MP4 remux (stream copy).
///
/// `input` must be a progressive M4B/M4A. Callers that want MP3 chapter files
/// should split the M4B first, then re-encode each chapter.
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
) -> Result<Vec<SplitChapterFile>> {
    if chapters.is_empty() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "split-by-chapter requested but no chapters available"
        )));
    }
    let input_ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(input_ext.as_str(), "mp3") {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "native chapter split requires M4B/M4A input; split before MP3 encode"
        )));
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
        let templates = options.naming_templates();
        let filename = chapter_storage_key_with_folder(
            folder_ctx,
            file_ctx,
            Some(templates.folder.as_str()),
            Some(templates.chapter_file.as_str()),
            &options.replacement_characters,
            chapter_no,
            &title,
            ext,
            options.path_limits,
        );
        let out_path = output_dir.join(filename.rsplit('/').next().unwrap_or(&filename));
        remux_trimmed_async(
            input,
            &out_path,
            TrimRange {
                start_ms,
                end_ms: Some(end_ms),
            },
        )
        .await
        .map_err(AcquireError::Media)?;
        outputs.push(SplitChapterFile {
            path: out_path,
            title,
        });
    }
    Ok(outputs)
}

/// Groups consecutive chapters so each file meets `minimum_file_duration_minutes` (ms); `0` keeps one chapter per file.
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
