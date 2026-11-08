//! Audio file-extension sniffing helpers for clear downloads.
//!
//! # Audience
//!
//! Source plugins and host acquire code naming downloaded parts.

/// Guess an audio file extension from a URL path (query/fragment stripped).
#[must_use]
pub fn extension_from_url(url: &str) -> Option<&'static str> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".m4b") {
        Some(".m4b")
    } else if path.ends_with(".m4a") || path.ends_with(".mp4") {
        Some(".m4a")
    } else if path.ends_with(".mp3") {
        Some(".mp3")
    } else if path.ends_with(".flac") {
        Some(".flac")
    } else if path.ends_with(".aac") {
        Some(".aac")
    } else if path.ends_with(".ogg") || path.ends_with(".oga") {
        Some(".ogg")
    } else {
        None
    }
}

/// Map a `Content-Type` header (parameters stripped) to an audio extension.
#[must_use]
pub fn extension_from_content_type(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    if ct.is_empty() || ct == "application/octet-stream" {
        return None;
    }
    match ct.as_str() {
        "audio/mpeg" | "audio/mp3" => return Some(".mp3"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => return Some(".m4a"),
        "audio/m4b" | "audio/x-m4b" => return Some(".m4b"),
        "audio/flac" | "audio/x-flac" => return Some(".flac"),
        "audio/ogg" | "application/ogg" | "audio/vorbis" => return Some(".ogg"),
        "audio/aac" | "audio/x-aac" => return Some(".aac"),
        _ => {}
    }
    if let Some(exts) = mime_guess::get_mime_extensions_str(&ct) {
        for ext in exts {
            match *ext {
                "mp3" => return Some(".mp3"),
                "m4b" => return Some(".m4b"),
                "m4a" | "mp4" => return Some(".m4a"),
                "flac" => return Some(".flac"),
                "ogg" | "oga" => return Some(".ogg"),
                "aac" => return Some(".aac"),
                _ => {}
            }
        }
    }
    None
}

/// Guess an audio extension from magic bytes (`infer`), with AAC/MP3 fallbacks.
#[must_use]
pub fn extension_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if let Some(kind) = infer::get(bytes) {
        match kind.mime_type() {
            "audio/mpeg" | "audio/mp3" => return Some(".mp3"),
            "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "video/mp4" => {
                // Prefer .m4b only when the URL said so; bytes alone → .m4a.
                return Some(".m4a");
            }
            "audio/flac" => return Some(".flac"),
            "audio/ogg" | "application/ogg" => return Some(".ogg"),
            "audio/aac" => return Some(".aac"),
            _ => {}
        }
        match kind.extension() {
            "mp3" => return Some(".mp3"),
            "m4a" | "mp4" => return Some(".m4a"),
            "flac" => return Some(".flac"),
            "ogg" => return Some(".ogg"),
            "aac" => return Some(".aac"),
            _ => {}
        }
    }
    // infer sometimes misses ID3-less MPEG frames / bare ftyp.
    if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        return Some(".m4a");
    }
    if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        return Some(".mp3");
    }
    if bytes.starts_with(b"fLaC") {
        return Some(".flac");
    }
    if bytes.starts_with(b"OggS") {
        return Some(".ogg");
    }
    None
}

/// Prefer byte sniffing, then `Content-Type`, then URL path; `default` when all fail.
#[must_use]
pub fn audio_extension(
    url: &str,
    bytes: Option<&[u8]>,
    content_type: Option<&str>,
    default: &'static str,
) -> &'static str {
    if let Some(bytes) = bytes {
        if let Some(ext) = extension_from_bytes(bytes) {
            // If URL explicitly says .m4b, keep that over generic mp4/m4a sniff.
            if ext == ".m4a" && extension_from_url(url) == Some(".m4b") {
                return ".m4b";
            }
            return ext;
        }
    }
    if let Some(ct) = content_type {
        if let Some(ext) = extension_from_content_type(ct) {
            if ext == ".m4a" && extension_from_url(url) == Some(".m4b") {
                return ".m4b";
            }
            return ext;
        }
    }
    extension_from_url(url).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_bytes_and_content_type() {
        assert_eq!(extension_from_url("https://x/a.m4b?sig=1"), Some(".m4b"));
        assert_eq!(
            audio_extension("https://x/a", Some(b"ID3\x03demo"), None, ".bin"),
            ".mp3"
        );
        let mut mp4 = vec![0u8; 12];
        mp4[4..8].copy_from_slice(b"ftyp");
        assert_eq!(
            audio_extension("https://cdn/track", Some(&mp4), None, ".bin"),
            ".m4a"
        );
        assert_eq!(
            extension_from_content_type("audio/mpeg; charset=binary"),
            Some(".mp3")
        );
        assert_eq!(
            audio_extension("https://cdn/x", None, Some("audio/flac"), ".bin"),
            ".flac"
        );
    }
}
