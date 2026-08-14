//! CUE sheets from Audible chapter_info.

use std::io::Write;
use std::path::Path;

use bookclerk_media::{
    brand_durations_from_chapter_info, rebase_chapters_after_brand_trim,
    runtime_length_ms_from_chapter_info, scale_chapters_to_duration,
};
use serde_json::Value;

use crate::error::Result;

/// One chapter with a start time in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatChapter {
    /// Chapter title after optional Unabridged/brand stripping.
    pub title: String,
    /// Start offset in milliseconds from the beginning of the (possibly rebased) audio.
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

/// Flatten + title-process Audible `chapter_info`, then rebase starts for plain
/// audio that has **no** Audible brand intro/outro (e.g. Libro.fm).
///
/// Always subtracts `brandIntroDurationMs` and drops chapters that fall in the
/// outro window — Libro/packaged M4B audio is already free of those segments.
///
/// When Audnexus omits `runtime_length_ms`, pass the probed plain-file duration
/// as `plain_audio_duration_ms` so outro chapters can still be trimmed
/// (`plain + intro + outro` reconstructs the Audible timeline).
#[must_use]
pub fn chapters_from_catalog_info_for_plain_audio(
    info: &Value,
    combine_nested: bool,
    merge_credits: bool,
    strip_unabridged: bool,
    strip_brand_titles: bool,
    plain_audio_duration_ms: Option<u64>,
) -> Vec<(String, u64)> {
    let brand = brand_durations_from_chapter_info(info);
    let mut runtime_ms = runtime_length_ms_from_chapter_info(info);
    if runtime_ms.is_none() {
        if let Some(plain) = plain_audio_duration_ms.filter(|d| *d > 0) {
            runtime_ms = Some(
                plain
                    .saturating_add(brand.intro_ms)
                    .saturating_add(brand.outro_ms),
            );
        }
    }
    let flat = process_chapter_titles(
        flatten_chapters(info),
        combine_nested,
        merge_credits,
        strip_unabridged,
        strip_brand_titles,
    );
    let pairs: Vec<(String, u64)> = flat.into_iter().map(|c| (c.title, c.start_ms)).collect();
    let rebased = rebase_chapters_after_brand_trim(&pairs, brand, runtime_ms);
    let content_ms = runtime_ms.map(|runtime| {
        runtime
            .saturating_sub(brand.intro_ms)
            .saturating_sub(brand.outro_ms)
    });
    scale_chapters_to_duration(&rebased, content_ms, plain_audio_duration_ms)
}

/// Clamp signed millisecond offsets to `u64` without wrapping negatives.
fn clamp_ms(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// Depth-first walk of nested `chapters` arrays; same-start parents are dropped later by dedup.
fn flatten_chapter_nodes(nodes: &[Value], out: &mut Vec<FlatChapter>) {
    for node in nodes {
        // Depth-first: emit nested leaves first, then this node. Parent/part
        // headings with distinct starts are kept so tree-aware players (and the
        // chapters.tree.json sidecar) retain hierarchy; same-start parents are
        // removed later by `dedup_by_key(start_ms)`.
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
                    .map(clamp_ms)
            })
            .or_else(|| node.get("startOffsetMs").and_then(Value::as_u64))
            .or_else(|| {
                node.get("startOffsetMs")
                    .and_then(Value::as_i64)
                    .map(clamp_ms)
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

/// Remap chapter-tree `startOffsetMs` values after flat-list alignment.
///
/// `start_map` keys are pre-align (rebased/scaled) starts; values are aligned
/// starts. Nested `chapters` arrays and titles are preserved. Brand intro/outro
/// fields are zeroed because plain audio does not include those segments.
#[must_use]
pub fn apply_start_map_to_chapter_tree(
    info: &Value,
    start_map: &std::collections::HashMap<u64, u64>,
) -> Value {
    let mut out = info.clone();
    if let Some(arr) = out.get_mut("chapters").and_then(Value::as_array_mut) {
        apply_start_map_to_nodes(arr, start_map);
    }
    if let Some(obj) = out.as_object_mut() {
        obj.insert("brandIntroDurationMs".into(), Value::from(0));
        obj.insert("brand_intro_duration_ms".into(), Value::from(0));
        obj.insert("brandOutroDurationMs".into(), Value::from(0));
        obj.insert("brand_outro_duration_ms".into(), Value::from(0));
    }
    out
}

/// Remaps each node's `startOffsetMs` / `start_offset_ms` using the flat-list alignment map.
fn apply_start_map_to_nodes(nodes: &mut [Value], start_map: &std::collections::HashMap<u64, u64>) {
    for node in nodes {
        if let Some(nested) = node.get_mut("chapters").and_then(Value::as_array_mut) {
            apply_start_map_to_nodes(nested, start_map);
        }
        let start_ms = node
            .get("start_offset_ms")
            .and_then(Value::as_u64)
            .or_else(|| node.get("startOffsetMs").and_then(Value::as_u64));
        let Some(old) = start_ms else {
            continue;
        };
        let Some(&new_start) = start_map.get(&old) else {
            continue;
        };
        if let Some(obj) = node.as_object_mut() {
            obj.insert("startOffsetMs".into(), Value::from(new_start));
            obj.insert("start_offset_ms".into(), Value::from(new_start));
        }
    }
}

/// Brand-rebase + duration-scale every node in an Audnexus chapter tree for
/// plain audio (no brand segments). Drops nodes that fall in the outro window.
/// Nesting is preserved for tree-aware players.
#[must_use]
pub fn rebase_chapter_tree_for_plain_audio(
    info: &Value,
    plain_audio_duration_ms: Option<u64>,
) -> Value {
    let brand = brand_durations_from_chapter_info(info);
    let mut runtime_ms = runtime_length_ms_from_chapter_info(info);
    if runtime_ms.is_none() {
        if let Some(plain) = plain_audio_duration_ms.filter(|d| *d > 0) {
            runtime_ms = Some(
                plain
                    .saturating_add(brand.intro_ms)
                    .saturating_add(brand.outro_ms),
            );
        }
    }
    let end_ms = runtime_ms
        .map(|r| r.saturating_sub(brand.outro_ms))
        .unwrap_or(u64::MAX);
    let content_ms = runtime_ms.map(|runtime| {
        runtime
            .saturating_sub(brand.intro_ms)
            .saturating_sub(brand.outro_ms)
    });
    let scale = match (content_ms, plain_audio_duration_ms) {
        (Some(content), Some(plain))
            if content > 0 && plain > 0 && content.abs_diff(plain) >= 250 && {
                let s = plain as f64 / content as f64;
                (0.98..=1.02).contains(&s)
            } =>
        {
            Some(plain as f64 / content as f64)
        }
        _ => None,
    };

    let mut out = info.clone();
    if let Some(arr) = out.get_mut("chapters").and_then(Value::as_array_mut) {
        *arr = rebase_tree_nodes(arr, brand.intro_ms, end_ms, scale);
    }
    if let Some(obj) = out.as_object_mut() {
        obj.insert("brandIntroDurationMs".into(), Value::from(0));
        obj.insert("brand_intro_duration_ms".into(), Value::from(0));
        obj.insert("brandOutroDurationMs".into(), Value::from(0));
        obj.insert("brand_outro_duration_ms".into(), Value::from(0));
        if let Some(plain) = plain_audio_duration_ms {
            obj.insert("runtimeLengthMs".into(), Value::from(plain));
            obj.insert("runtime_length_ms".into(), Value::from(plain));
        }
    }
    out
}

/// Subtracts brand intro, drops outro-window nodes, and optionally scales starts to plain duration.
fn rebase_tree_nodes(
    nodes: &[Value],
    intro_ms: u64,
    end_ms: u64,
    scale: Option<f64>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for node in nodes {
        let start_ms = node
            .get("start_offset_ms")
            .and_then(Value::as_u64)
            .or_else(|| {
                node.get("start_offset_ms")
                    .and_then(Value::as_i64)
                    .map(clamp_ms)
            })
            .or_else(|| node.get("startOffsetMs").and_then(Value::as_u64))
            .or_else(|| {
                node.get("startOffsetMs")
                    .and_then(Value::as_i64)
                    .map(clamp_ms)
            })
            .unwrap_or(0);
        if start_ms >= end_ms {
            continue;
        }
        let mut rebased = if start_ms < intro_ms {
            0
        } else {
            start_ms.saturating_sub(intro_ms)
        };
        if let Some(s) = scale {
            rebased = ((rebased as f64) * s).round().max(0.0) as u64;
        }
        let mut cloned = node.clone();
        if let Some(nested) = node.get("chapters").and_then(Value::as_array) {
            let kids = rebase_tree_nodes(nested, intro_ms, end_ms, scale);
            if let Some(obj) = cloned.as_object_mut() {
                if kids.is_empty() {
                    obj.remove("chapters");
                } else {
                    obj.insert("chapters".into(), Value::Array(kids));
                }
            }
        }
        if let Some(obj) = cloned.as_object_mut() {
            obj.insert("startOffsetMs".into(), Value::from(rebased));
            obj.insert("start_offset_ms".into(), Value::from(rebased));
        }
        out.push(cloned);
    }
    out
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

/// Formats milliseconds as CUE `HH:MM:SS:FF` (75 frames/sec, frame clamped to 74).
fn ms_to_cue_time(ms: u64) -> String {
    let total_secs = ms / 1000;
    let frames = ((ms % 1000) * 75 / 1000).min(74);
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}:{frames:02}")
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
    fn clamps_negative_start_offsets_to_zero() {
        let info = serde_json::json!({
            "chapters": [
                {"title": "Bad", "start_offset_ms": -1},
                {"title": "Camel", "startOffsetMs": -50},
                {"title": "Ok", "start_offset_ms": 1000}
            ]
        });
        let flat = flatten_chapters(&info);
        // Negatives clamp to 0; flatten_chapters also dedupes by start_ms.
        assert!(flat.iter().any(|c| c.start_ms == 0));
        assert!(flat.iter().any(|c| c.start_ms == 1000));
        assert!(flat.iter().all(|c| c.start_ms < 1_000_000_000_000));
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

    #[test]
    fn plain_audio_rebases_audible_tree_past_brand_intro() {
        let info = serde_json::json!({
            "brandIntroDurationMs": 4_000,
            "brandOutroDurationMs": 5_000,
            "runtime_length_ms": 3_600_000,
            "chapters": [
                {"title": "Opening Credits", "start_offset_ms": 0, "length_ms": 8_000},
                {
                    "title": "Part 1: Eto Demerzel",
                    "start_offset_ms": 8_000,
                    "length_ms": 2_000,
                    "chapters": [
                        {"title": "Chapter 1", "start_offset_ms": 10_000, "length_ms": 90_000},
                        {"title": "Chapter 2", "start_offset_ms": 100_000, "length_ms": 90_000}
                    ]
                },
                {"title": "End Credits", "start_offset_ms": 3_596_000, "length_ms": 4_000}
            ]
        });
        let out =
            chapters_from_catalog_info_for_plain_audio(&info, false, false, false, false, None);
        assert_eq!(out[0], ("Opening Credits".into(), 0));
        // Part heading kept (distinct start from child chapters).
        assert!(
            out.iter()
                .any(|(t, s)| t == "Part 1: Eto Demerzel" && *s == 4_000),
            "{out:?}"
        );
        assert!(
            out.iter().any(|(t, s)| t == "Chapter 1" && *s == 6_000),
            "{out:?}"
        );
        assert!(
            out.iter().any(|(t, s)| t == "Chapter 2" && *s == 96_000),
            "{out:?}"
        );
        // End Credits starts at/after the outro window and is dropped.
        assert!(out.iter().all(|(t, _)| t != "End Credits"), "{out:?}");
    }

    #[test]
    fn rebases_nested_tree_preserving_hierarchy() {
        let info = serde_json::json!({
            "brandIntroDurationMs": 4_000,
            "brandOutroDurationMs": 5_000,
            "runtimeLengthMs": 3_600_000,
            "chapters": [
                {"title": "Opening Credits", "startOffsetMs": 0},
                {
                    "title": "Part 1",
                    "startOffsetMs": 8_000,
                    "chapters": [
                        {"title": "Chapter 1", "startOffsetMs": 10_000},
                        {"title": "Chapter 2", "startOffsetMs": 100_000}
                    ]
                },
                {"title": "End Credits", "startOffsetMs": 3_596_000}
            ]
        });
        let tree = rebase_chapter_tree_for_plain_audio(&info, Some(3_591_000));
        let parts = tree["chapters"].as_array().unwrap();
        assert!(parts.iter().all(|n| n["title"] != "End Credits"));
        let part = parts.iter().find(|n| n["title"] == "Part 1").unwrap();
        assert_eq!(part["startOffsetMs"], 4_000);
        assert_eq!(part["chapters"][0]["title"], "Chapter 1");
        assert_eq!(part["chapters"][0]["startOffsetMs"], 6_000);
        assert_eq!(tree["brandIntroDurationMs"], 0);
    }

    #[test]
    fn plain_audio_uses_probed_duration_when_runtime_missing() {
        let info = serde_json::json!({
            "brandIntroDurationMs": 4_000,
            "brandOutroDurationMs": 5_000,
            "chapters": [
                {"title": "Opening Credits", "start_offset_ms": 0},
                {"title": "Chapter 1", "start_offset_ms": 10_000},
                {"title": "End Credits", "start_offset_ms": 3_596_000}
            ]
        });
        // Plain file duration = Audible runtime - intro - outro = 3_600_000 - 4k - 5k.
        let plain_ms = 3_600_000u64 - 4_000 - 5_000;
        let out = chapters_from_catalog_info_for_plain_audio(
            &info,
            false,
            false,
            false,
            false,
            Some(plain_ms),
        );
        assert!(out.iter().any(|(t, _)| t == "Chapter 1"), "{out:?}");
        assert!(out.iter().all(|(t, _)| t != "End Credits"), "{out:?}");
    }
}
