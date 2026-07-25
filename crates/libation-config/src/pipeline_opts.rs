//! Liberate ingest quality and output formatting knobs.

use serde::{Deserialize, Serialize};

/// Whether / which chapter JSON sidecars to write beside liberated audio.
///
/// Default is [`Self::Off`]. Replaces the old `save_chapter_json` bool +
/// `chapter_layout` pair for *sidecar output* (Audible API fetch layout is
/// chosen separately when chapters are needed for embedding).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChapterJsonMode {
    /// Do not write chapter JSON sidecars.
    #[default]
    Off,
    /// Write `chapters.flat.json` only.
    Flat,
    /// Write `chapters.tree.json` (nested) only.
    Tree,
    /// Write both flat and tree sidecars.
    Both,
}

impl ChapterJsonMode {
    #[must_use]
    pub fn wants_flat(self) -> bool {
        matches!(self, Self::Flat | Self::Both)
    }

    #[must_use]
    pub fn wants_tree(self) -> bool {
        matches!(self, Self::Tree | Self::Both)
    }

    #[must_use]
    pub fn wants_any(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Parse `off|flat|tree|both`, or legacy bool-ish values (`true`→tree, `false`→off).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" | "0" | "no" => Some(Self::Off),
            "flat" => Some(Self::Flat),
            "tree" | "true" | "1" | "yes" => Some(Self::Tree),
            "both" | "all" => Some(Self::Both),
            _ => None,
        }
    }
}

/// Post-download output formatting. Default is enriched M4B (tags + chapters).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Remux/transcode to M4B and embed metadata/chapters (default).
    #[default]
    EnrichedM4b,
    /// Single MP3 file (LAME re-encode).
    SingleMp3,
    /// One MP3 per chapter (after M4B chapter split).
    SplitMp3ByChapter,
    /// Split MP3 by target file size (see `split_mp3_max_mb`).
    SplitMp3BySize,
    /// Opus output (not yet implemented).
    Opus,
    /// Leave store-delivered media as-is (no remux/transcode/re-encode).
    None,
}

impl OutputFormat {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "enriched_m4b" | "m4b" | "audiobook" => Some(Self::EnrichedM4b),
            "single_mp3" | "mp3" => Some(Self::SingleMp3),
            "split_mp3_by_chapter" | "split_by_chapter" | "chapter_mp3" => {
                Some(Self::SplitMp3ByChapter)
            }
            "split_mp3_by_size" | "split_by_size" => Some(Self::SplitMp3BySize),
            "opus" => Some(Self::Opus),
            "none" | "noop" | "as_is" | "passthrough" => Some(Self::None),
            _ => None,
        }
    }

    #[must_use]
    pub fn wants_mp3(self) -> bool {
        matches!(
            self,
            Self::SingleMp3 | Self::SplitMp3ByChapter | Self::SplitMp3BySize
        )
    }

    #[must_use]
    pub fn wants_split_by_chapter(self) -> bool {
        matches!(self, Self::SplitMp3ByChapter)
    }

    #[must_use]
    pub fn wants_split_by_size(self) -> bool {
        matches!(self, Self::SplitMp3BySize)
    }

    #[must_use]
    pub fn is_noop(self) -> bool {
        matches!(self, Self::None)
    }

    #[must_use]
    pub fn wants_opus(self) -> bool {
        matches!(self, Self::Opus)
    }

    #[must_use]
    pub fn prefers_m4b_container(self) -> bool {
        matches!(self, Self::EnrichedM4b | Self::SplitMp3ByChapter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_json_parse() {
        assert_eq!(ChapterJsonMode::parse("off"), Some(ChapterJsonMode::Off));
        assert_eq!(ChapterJsonMode::parse("BOTH"), Some(ChapterJsonMode::Both));
        assert_eq!(ChapterJsonMode::parse("true"), Some(ChapterJsonMode::Tree));
    }

    #[test]
    fn output_parse() {
        assert_eq!(
            OutputFormat::parse("enriched_m4b"),
            Some(OutputFormat::EnrichedM4b)
        );
        assert_eq!(OutputFormat::parse("none"), Some(OutputFormat::None));
        assert_eq!(
            OutputFormat::parse("split_mp3_by_chapter"),
            Some(OutputFormat::SplitMp3ByChapter)
        );
    }
}
