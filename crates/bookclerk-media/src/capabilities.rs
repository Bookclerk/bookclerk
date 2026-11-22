//! What this build of the media worker can read and write.
//!
//! Hosts ask here before handing the worker audio, instead of keeping their
//! own list of formats: the acquire pipeline takes a library's copy from
//! another store only when an output that re-encodes or repackages audio
//! can read it. The answers follow the crate's decode front: the
//! Symphonia features this crate enables, FDK for every AAC profile,
//! libopus for Opus and, in builds with the `libav` feature (every release
//! and Docker build), FFmpeg for AC-3, E-AC-3, WMA and
//! AMR. The daemon serves the same report at
//! `GET /api/status/media` and the CLI prints it with
//! `bookclerk media capabilities`.

use bookclerk_tags::AudioCodec;

/// Media types of whole files the worker opens: MP4 (`.m4a`/`.m4b`, AAC
/// or ALAC), MPEG audio layers I–III, FLAC, ADTS AAC, WAV, AIFF, Core
/// Audio Format and Ogg (Opus or Vorbis). The container is still sniffed from the bytes when the
/// file is opened; this list only says which kinds are worth sending.
///
/// Builds with the `libav` feature also open WMA (ASF), raw AC-3 and
/// E-AC-3, and AMR (raw or in 3GP).
#[cfg(not(feature = "libav"))]
pub const READABLE_CONTENT_TYPES: &[&str] = &[
    "audio/mp4",
    "audio/mpeg",
    "audio/flac",
    "audio/aac",
    "audio/wav",
    "audio/aiff",
    "audio/x-caf",
    "audio/ogg",
    "audio/opus",
];

/// Media types of whole files the worker opens: MP4 (`.m4a`/`.m4b`, AAC,
/// ALAC or E-AC-3), MPEG audio layers I–III, FLAC, ADTS AAC, WAV, AIFF,
/// Core Audio Format, Ogg (Opus or Vorbis), and through FFmpeg WMA (ASF),
/// raw AC-3 and E-AC-3, and AMR (raw or in 3GP). The container is still
/// sniffed from the bytes when the file is opened; this list only says
/// which kinds are worth sending.
#[cfg(feature = "libav")]
pub const READABLE_CONTENT_TYPES: &[&str] = &[
    "audio/mp4",
    "audio/mpeg",
    "audio/flac",
    "audio/aac",
    "audio/wav",
    "audio/aiff",
    "audio/x-caf",
    "audio/ogg",
    "audio/opus",
    "audio/x-ms-wma",
    "audio/ac3",
    "audio/eac3",
    "audio/amr",
    "audio/amr-wb",
    "audio/3gpp",
];

/// What the worker produces from decoded audio.
pub const OUTPUT_ENCODINGS: &[OutputEncoding] = &[
    OutputEncoding::AacLcM4b,
    OutputEncoding::Mp3,
    OutputEncoding::Opus,
];

/// One encoding the worker can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputEncoding {
    /// AAC-LC in an M4B, through FDK.
    AacLcM4b,
    /// MP3, through LAME.
    Mp3,
    /// Opus in an Ogg file, through libopus.
    Opus,
}

impl OutputEncoding {
    /// The output format id that selects this encoding (`m4b`, `mp3`,
    /// `opus`), as copy formats spell it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::AacLcM4b => "m4b",
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
        }
    }

    /// True for an encoding settings pages offer only under their advanced
    /// options: one that fewer players open than M4B and MP3.
    #[must_use]
    pub fn is_advanced(self) -> bool {
        match self {
            Self::AacLcM4b | Self::Mp3 => false,
            Self::Opus => true,
        }
    }
}

/// The codecs, containers and output encodings this build handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Audio encodings the worker decodes, in [`AudioCodec`] order.
    pub decodes: Vec<AudioCodec>,
    /// Media types of files the worker opens ([`READABLE_CONTENT_TYPES`]).
    pub reads: &'static [&'static str],
    /// Encodings the worker writes ([`OUTPUT_ENCODINGS`]).
    pub encodes: &'static [OutputEncoding],
}

/// Every [`AudioCodec`] the tag reader names, so [`capabilities`] can list
/// the decodable ones. [`decodes`] matches exhaustively, so a new codec
/// fails to compile there until it is decided.
const ALL_CODECS: [AudioCodec; 8] = [
    AudioCodec::Aac,
    AudioCodec::Alac,
    AudioCodec::Flac,
    AudioCodec::Mp3,
    AudioCodec::Opus,
    AudioCodec::Vorbis,
    AudioCodec::Eac3,
    AudioCodec::Ac4,
];

/// True when the worker decodes audio encoded as `codec`.
///
/// AAC covers every profile (LC, HE, HE v2, xHE). MP3 covers MPEG audio
/// layers I and II too, which the tag reader reports as MP3. Opus covers
/// mono and stereo streams; surround Opus (several streams) fails to open.
/// E-AC-3 decodes in builds with the `libav` feature. No build decodes
/// AC-4.
#[must_use]
pub fn decodes(codec: AudioCodec) -> bool {
    match codec {
        AudioCodec::Aac
        | AudioCodec::Alac
        | AudioCodec::Flac
        | AudioCodec::Mp3
        | AudioCodec::Opus
        | AudioCodec::Vorbis => true,
        AudioCodec::Eac3 => cfg!(feature = "libav"),
        AudioCodec::Ac4 => false,
    }
}

/// True when the worker opens files of media type `content_type` (compared
/// without case or parameters).
#[must_use]
pub fn reads_content_type(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or_default().trim();
    READABLE_CONTENT_TYPES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(essence))
}

/// The report for this build.
#[must_use]
pub fn capabilities() -> Capabilities {
    Capabilities {
        decodes: ALL_CODECS.into_iter().filter(|c| decodes(*c)).collect(),
        reads: READABLE_CONTENT_TYPES,
        encodes: OUTPUT_ENCODINGS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_lists_what_decodes() {
        let report = capabilities();
        let mut want = vec![
            AudioCodec::Aac,
            AudioCodec::Alac,
            AudioCodec::Flac,
            AudioCodec::Mp3,
            AudioCodec::Opus,
            AudioCodec::Vorbis,
        ];
        if cfg!(feature = "libav") {
            want.push(AudioCodec::Eac3);
        }
        assert_eq!(report.decodes, want);
        assert!(!decodes(AudioCodec::Ac4));
        assert_eq!(report.encodes, OUTPUT_ENCODINGS);
        let names: Vec<_> = report.encodes.iter().map(|e| e.name()).collect();
        assert_eq!(names, ["m4b", "mp3", "opus"]);
        assert!(OutputEncoding::Opus.is_advanced());
        assert!(!OutputEncoding::Mp3.is_advanced());
    }

    #[test]
    fn content_types_compare_by_essence() {
        assert!(reads_content_type("audio/flac"));
        assert!(reads_content_type("Audio/MP4; codecs=mp4a.40.2"));
        assert!(reads_content_type("audio/ogg"));
        assert!(!reads_content_type("audio/x-matroska"));
        assert_eq!(
            reads_content_type("audio/x-ms-wma"),
            cfg!(feature = "libav")
        );
        assert!(!reads_content_type(""));
    }
}
