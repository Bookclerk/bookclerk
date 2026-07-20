//! CUE sheets and ffmpeg chapter metadata from Audible chapter_info.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

use crate::error::Result;

/// One chapter with a start time in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatChapter {
    pub title: String,
    pub start_ms: u64,
}

/// Flatten Audible `chapter_info` (tree or flat) into ordered chapters.
#[must_use]
pub fn flatten_chapters(info: &Value) -> Vec<FlatChapter> {
    let mut out = Vec::new();
    if let Some(arr) = info.get("chapters").and_then(Value::as_array) {
        flatten_chapter_nodes(arr, &mut out);
    }
    out.sort_by_key(|c| c.start_ms);
    out.dedup_by_key(|c| c.start_ms);
    out
}

/// Apply classic chapter title post-processing options.
#[must_use]
pub fn process_chapter_titles(
    mut chapters: Vec<FlatChapter>,
    combine_nested: bool,
    merge_credits: bool,
    strip_unabridged: bool,
    strip_brand: bool,
) -> Vec<FlatChapter> {
    for ch in &mut chapters {
        let mut title = ch.title.clone();
        if strip_unabridged {
            title = title.replace("(Unabridged)", "").replace("Unabridged", "");
        }
        if strip_brand {
            title = title
                .replace("Audible Studios", "")
                .replace("Audible, Inc.", "");
        }
        if merge_credits {
            let lower = title.to_ascii_lowercase();
            if lower.contains("opening credits") || lower.contains("end credits") {
                title = title.replace("Opening Credits", "Credits");
                title = title.replace("End Credits", "Credits");
            }
        }
        if combine_nested && title.contains(':') {
            title = title
                .split(':')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" - ");
        }
        ch.title = title.trim().to_string();
    }
    chapters.retain(|c| !c.title.is_empty());
    chapters
}

fn flatten_chapter_nodes(nodes: &[Value], out: &mut Vec<FlatChapter>) {
    for node in nodes {
        if let Some(nested) = node.get("chapters").and_then(Value::as_array) {
            flatten_chapter_nodes(nested, out);
        }
        let Some(title) = node.get("title").and_then(Value::as_str) else {
            continue;
        };
        let start_ms = node
            .get("start_offset_ms")
            .and_then(Value::as_u64)
            .or_else(|| {
                node.get("start_offset_ms")
                    .and_then(Value::as_i64)
                    .map(|n| n as u64)
            })
            .unwrap_or(0);
        if !title.trim().is_empty() {
            out.push(FlatChapter {
                title: title.trim().to_string(),
                start_ms,
            });
        }
    }
}

/// Write a classic `.cue` sidecar next to the audio file.
pub fn write_cue(
    path: &Path,
    audio_filename: &str,
    performer: &str,
    album_title: &str,
    chapters: &[FlatChapter],
) -> Result<()> {
    if chapters.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "PERFORMER {performer:?}")?;
    writeln!(file, "TITLE {album_title:?}")?;
    writeln!(file, "FILE {audio_filename:?} WAVE")?;
    for (idx, ch) in chapters.iter().enumerate() {
        writeln!(file, "  TRACK {:02} AUDIO", idx + 1)?;
        writeln!(file, "    TITLE {title:?}", title = ch.title)?;
        writeln!(file, "    INDEX 01 {}", ms_to_cue_time(ch.start_ms))?;
    }
    Ok(())
}

fn ms_to_cue_time(ms: u64) -> String {
    let total_secs = ms / 1000;
    let frames = ((ms % 1000) * 75 / 1000).min(74);
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}:{frames:02}")
}

/// Write an ffmpeg ffmetadata file with `[CHAPTER]` entries.
pub fn write_ffmetadata(
    path: &Path,
    title: &str,
    artist: &str,
    narrator: Option<&str>,
    chapters: &[FlatChapter],
    total_duration_ms: Option<u64>,
) -> Result<()> {
    if chapters.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    writeln!(file, ";FFMETADATA1")?;
    writeln!(file, "title={title}")?;
    writeln!(file, "artist={artist}")?;
    if let Some(narrator) = narrator.filter(|s| !s.is_empty()) {
        writeln!(file, "composer={narrator}")?;
    }
    for (idx, ch) in chapters.iter().enumerate() {
        let end = chapters
            .get(idx + 1)
            .map(|next| next.start_ms)
            .or(total_duration_ms)
            .unwrap_or(ch.start_ms.saturating_add(1));
        writeln!(file, "[CHAPTER]")?;
        writeln!(file, "TIMEBASE=1/1000")?;
        writeln!(file, "START={}", ch.start_ms)?;
        writeln!(file, "END={end}")?;
        writeln!(file, "title={}", escape_ffmeta(&ch.title))?;
    }
    Ok(())
}

fn escape_ffmeta(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('=', "\\=")
        .replace(';', "\\;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_tree_chapters() {
        let info = serde_json::json!({
            "chapters": [
                {
                    "title": "Part 1",
                    "chapters": [
                        {"title": "Intro", "start_offset_ms": 0},
                        {"title": "Chapter 1", "start_offset_ms": 60000}
                    ]
                }
            ]
        });
        let flat = flatten_chapters(&info);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].title, "Intro");
        assert_eq!(flat[1].start_ms, 60000);
    }

    #[test]
    fn writes_cue_with_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.cue");
        write_cue(
            &path,
            "book.m4b",
            "Author",
            "Title",
            &[
                FlatChapter {
                    title: "One".into(),
                    start_ms: 0,
                },
                FlatChapter {
                    title: "Two".into(),
                    start_ms: 125_000,
                },
            ],
        )
        .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("FILE \"book.m4b\" WAVE"));
        assert!(text.contains("TRACK 01 AUDIO"));
        assert!(text.contains("TITLE \"Two\""));
    }
}
