//! Download Libro.fm title materials (M4B or MP3 zip parts) into a cache dir.

use std::fs::File;
use std::io::{copy, Cursor, Write};
use std::path::{Path, PathBuf};

use bookclerk_source::{PlainAudioPart, PlainFetch};

use crate::container::LibroContainer;
use zip::ZipArchive;

use crate::client::{DownloadManifest, DownloadPart, LibroClient, ManifestFormat, ManifestTrack};
use crate::error::{LibroError, Result};

/// Fetch one ISBN into `cache_dir`, preferring M4B when available.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn fetch_title_materials(
    client: &LibroClient,
    isbn: &str,
    cache_dir: &Path,
) -> Result<PlainFetch> {
    fetch_title_materials_with(client, isbn, cache_dir, LibroContainer::M4b).await
}

/// Fetch one ISBN into `cache_dir` using the preferred container.
///
/// [`LibroContainer::M4b`] (default):
/// 1. `download-manifest?format=m4b` — single M4B part + chapter tracks
/// 2. `audiobooks/{isbn}/packaged_m4b` — legacy/alternate M4B URL
/// 3. `download-manifest` without format — multi-part ZIP of MP3s (fallback)
///
/// [`LibroContainer::Zip`]: skip M4B and go straight to ZIP / MP3 parts.
pub async fn fetch_title_materials_with(
    client: &LibroClient,
    isbn: &str,
    cache_dir: &Path,
    container: LibroContainer,
) -> Result<PlainFetch> {
    std::fs::create_dir_all(cache_dir)?;
    let title_dir = cache_dir.join(isbn);
    std::fs::create_dir_all(&title_dir)?;

    if matches!(container, LibroContainer::M4b) {
        // 1) Prefer format=m4b (APK LibroDownloadManager uses MediaFormat.M4B).
        match client.download_manifest(isbn, ManifestFormat::M4b).await {
            Ok(manifest) => {
                if let Some(url) = first_m4b_part_url(&manifest.parts) {
                    let m4b_path = download_m4b(client, url, &title_dir).await?;
                    return Ok(PlainFetch {
                        parts: Vec::new(),
                        m4b_path: Some(m4b_path),
                        cover_path: None,
                        chapters: chapters_from_tracks(&manifest.tracks),
                        pdf_url: None,
                    });
                }
                // Server ignored format / returned zips — use those parts.
                if parts_look_like_zip(&manifest.parts) && !manifest.parts.is_empty() {
                    let parts = download_mp3_parts(client, &manifest, &title_dir).await?;
                    return Ok(PlainFetch {
                        parts,
                        m4b_path: None,
                        cover_path: None,
                        chapters: chapters_from_tracks(&manifest.tracks),
                        pdf_url: None,
                    });
                }
            }
            Err(err) => {
                tracing::debug!(%isbn, error = %err, "format=m4b download-manifest failed");
            }
        }

        // 2) Legacy packaged_m4b endpoint (same CDN object when present).
        if let Some(m4b) = client.packaged_m4b(isbn).await? {
            let m4b_path = download_m4b(client, &m4b.m4b_url, &title_dir).await?;
            let chapters = match client.download_manifest(isbn, ManifestFormat::Zip).await {
                Ok(manifest) => chapters_from_tracks(&manifest.tracks),
                Err(err) => {
                    tracing::debug!(%isbn, error = %err, "manifest unavailable after M4B download");
                    Vec::new()
                }
            };
            return Ok(PlainFetch {
                parts: Vec::new(),
                m4b_path: Some(m4b_path),
                cover_path: None,
                chapters,
                pdf_url: None,
            });
        }
    }

    // ZIP / MP3 parts (preferred when container=zip, or M4B fallback).
    let manifest = client.download_manifest(isbn, ManifestFormat::Zip).await?;
    let parts = download_mp3_parts(client, &manifest, &title_dir).await?;
    Ok(PlainFetch {
        parts,
        m4b_path: None,
        cover_path: None,
        chapters: chapters_from_tracks(&manifest.tracks),
        pdf_url: None,
    })
}

/// First manifest part whose path (before query) ends with `.m4b`.
fn first_m4b_part_url(parts: &[DownloadPart]) -> Option<&str> {
    parts.iter().find_map(|p| {
        let url = p.url.as_str();
        let path = url.split(['?', '#']).next().unwrap_or(url);
        if path.to_ascii_lowercase().ends_with(".m4b") {
            Some(url)
        } else {
            None
        }
    })
}

/// True when any part URL path ends with `.zip` (server ignored `format=m4b`).
fn parts_look_like_zip(parts: &[DownloadPart]) -> bool {
    parts.iter().any(|p| {
        let path = p.url.split(['?', '#']).next().unwrap_or(p.url.as_str());
        path.to_ascii_lowercase().ends_with(".zip")
    })
}

/// Downloads one M4B into `title_dir`, using the URL filename or `book.m4b`.
async fn download_m4b(client: &LibroClient, url: &str, title_dir: &Path) -> Result<PathBuf> {
    let bytes = client.download_bytes(url).await?;
    let filename = filename_from_url(url).unwrap_or_else(|| "book.m4b".into());
    let path = title_dir.join(sanitize_filename(&filename));
    std::fs::write(&path, &bytes)?;
    Ok(path)
}

/// Downloads each manifest part, extracting zip audio or writing a sniffed file.
async fn download_mp3_parts(
    client: &LibroClient,
    manifest: &DownloadManifest,
    title_dir: &Path,
) -> Result<Vec<PlainAudioPart>> {
    if manifest.parts.is_empty() {
        return Err(LibroError::download("download-manifest returned no parts"));
    }

    let parts_dir = title_dir.join("parts");
    std::fs::create_dir_all(&parts_dir)?;
    let mut audio_parts = Vec::new();

    for (idx, part) in manifest.parts.iter().enumerate() {
        let bytes = client.download_bytes(&part.url).await?;
        let extracted = extract_or_write_part(&bytes, &parts_dir, idx)?;
        for path in extracted {
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string);
            audio_parts.push(PlainAudioPart {
                path,
                title,
                duration_ms: None,
            });
        }
    }

    audio_parts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(audio_parts)
}

/// Extracts audio from a zip part, or writes raw bytes with a sniffed extension.
fn extract_or_write_part(bytes: &[u8], parts_dir: &Path, idx: usize) -> Result<Vec<PathBuf>> {
    match ZipArchive::new(Cursor::new(bytes)) {
        Ok(mut archive) => {
            let out = extract_zip_audio(&mut archive, parts_dir, idx)?;
            if out.is_empty() {
                let zip_path = parts_dir.join(format!("part-{idx:03}.zip"));
                std::fs::write(&zip_path, bytes)?;
                return Err(LibroError::download(format!(
                    "zip part {idx} contained no audio files (saved at {})",
                    zip_path.display()
                )));
            }
            Ok(out)
        }
        Err(_) => {
            let ext = sniff_audio_ext(bytes).unwrap_or("bin");
            let path = parts_dir.join(format!("part-{idx:03}.{ext}"));
            std::fs::write(&path, bytes)?;
            Ok(vec![path])
        }
    }
}

/// Writes zip members whose names look like audio; skips directories and other files.
fn extract_zip_audio<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    parts_dir: &Path,
    idx: usize,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| LibroError::download(format!("zip entry: {e}")))?;
        if file.is_dir() {
            continue;
        }
        let name = file
            .enclosed_name()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(format!("part-{idx}-{i}")));
        let file_name = name
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("track.bin");
        if !is_audio_filename(file_name) {
            continue;
        }
        let dest = parts_dir.join(format!("{:03}-{file_name}", idx + 1));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut dest_file = File::create(&dest)?;
        copy(&mut file, &mut dest_file)?;
        dest_file.flush()?;
        out.push(dest);
    }
    Ok(out)
}

/// Chapters from tracks.
#[must_use]
pub fn chapters_from_tracks(tracks: &[ManifestTrack]) -> Vec<(String, u64)> {
    let mut chapters = Vec::new();
    let mut offset_ms = 0u64;
    for (i, track) in tracks.iter().enumerate() {
        let title = track
            .chapter_title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Chapter {}", track.number.unwrap_or((i + 1) as u32)));
        chapters.push((title, offset_ms));
        offset_ms = offset_ms.saturating_add(track.duration_ms());
    }
    chapters
}

/// True when `name` ends with a known audio extension (mp3, m4a/m4b, aac, flac, ogg).
fn is_audio_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".mp3")
        || lower.ends_with(".m4a")
        || lower.ends_with(".m4b")
        || lower.ends_with(".aac")
        || lower.ends_with(".flac")
        || lower.ends_with(".ogg")
}

/// Magic-byte audio extension without the leading dot, or `None` if unknown.
fn sniff_audio_ext(bytes: &[u8]) -> Option<&'static str> {
    bookclerk_source::extension_from_bytes(bytes).map(|ext| ext.trim_start_matches('.'))
}

/// Filename from `response-content-disposition` or the last path segment.
fn filename_from_url(url: &str) -> Option<String> {
    if let Some(q) = url.split_once('?').map(|(_, q)| q) {
        for pair in q.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            if k == "response-content-disposition" {
                let decoded = percent_decode(v);
                if let Some(name) = content_disposition_filename(&decoded) {
                    return Some(name);
                }
            }
        }
    }
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    path.rsplit('/')
        .next()
        .map(percent_decode)
        .filter(|s| !s.is_empty() && s.contains('.'))
}

/// Extracts the `filename=` token from a Content-Disposition value (`+` becomes space).
fn content_disposition_filename(header: &str) -> Option<String> {
    // filename="Book+Title.m4b" or filename=Book.m4b
    let lower = header.to_ascii_lowercase();
    let key = "filename=";
    let idx = lower.find(key)?;
    let mut rest = header[idx + key.len()..].trim();
    if let Some(stripped) = rest.strip_prefix('"') {
        rest = stripped;
        if let Some(end) = rest.find('"') {
            rest = &rest[..end];
        }
    } else if let Some(end) = rest.find(';') {
        rest = &rest[..end];
    }
    let name = rest.replace('+', " ");
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Percent-decodes a path or query fragment; returns the raw string on failure.
fn percent_decode(s: &str) -> String {
    // Minimal decode for path segments; fall back to raw on failure.
    match urlencoding_lite(s) {
        Some(v) => v,
        None => s.to_string(),
    }
}

/// Decodes `%HH` and `+` (as space); `None` if the hex or UTF-8 is invalid.
fn urlencoding_lite(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Replaces path-separator and reserved characters so the name is safe on disk.
fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_matches('.');
    if trimmed.is_empty() {
        "download.bin".into()
    } else {
        trimmed.to_string()
    }
}
