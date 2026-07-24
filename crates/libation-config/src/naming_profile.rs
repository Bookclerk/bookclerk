//! Named path-template profiles for liberated audiobook layouts.
//!
//! Profiles are presets for `folder_template` / `file_template` /
//! `chapter_file_template`. Explicit template overrides in config still win
//! per field. New consumer layouts can be added here without changing the
//! naming engine.

use serde::{Deserialize, Serialize};

/// Preset path-template profile.
///
/// Select via `download.naming_profile`. Individual `folder_template` /
/// `file_template` / `chapter_file_template` values override the profile when
/// set.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NamingProfile {
    /// [Audiobookshelf](https://audiobookshelf.org/docs/documentation/libraries/book-library/directory-structure/)
    /// recommended layout: `{Author}/{Series}/{Book}` or `{Author}/{Book}`,
    /// with series sequence, publish year, short title, and narrator braces in
    /// the book folder when available; file is full title with ASIN/ISBN.
    #[default]
    Audiobookshelf,
    /// Previous Libation desktop defaults (`Templates.*.DefaultTemplate`).
    Classic,
}

/// Concrete templates contributed by a [`NamingProfile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamingProfileTemplates {
    pub folder: &'static str,
    pub file: &'static str,
    pub chapter_file: &'static str,
}

impl NamingProfile {
    /// All known profiles (for CLI listing).
    #[must_use]
    pub fn all() -> &'static [NamingProfile] {
        &[NamingProfile::Audiobookshelf, NamingProfile::Classic]
    }

    /// Stable config / CLI id (`audiobookshelf`, `classic`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audiobookshelf => "audiobookshelf",
            Self::Classic => "classic",
        }
    }

    /// Short human description of the profile's layout intent.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Audiobookshelf => {
                "Audiobookshelf Author/{FirstSeries/}{N - }{YYYY - }Short Title {Narrator}/Title [ASIN]"
            }
            Self::Classic => "Classic Libation <title short> [<id>]/<title> [<id>]",
        }
    }

    /// Parse a profile id (case-insensitive). Accepts common aliases.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "audiobookshelf" | "abs" | "bookshelf" => Some(Self::Audiobookshelf),
            "classic" | "libation" | "default-classic" => Some(Self::Classic),
            _ => None,
        }
    }

    /// Templates for this profile.
    #[must_use]
    pub fn templates(self) -> NamingProfileTemplates {
        match self {
            // Author/{Series/}{N - }{YYYY - }Short Title {Narrator}/Full Title [ASIN]
            // Matches Audiobookshelf Author/Series/Book (or Author/Book) scanning,
            // with series sequence, publish year, and narrator braces in the book
            // folder name when available.
            Self::Audiobookshelf => NamingProfileTemplates {
                folder: "<first author>/<has series-><first series>/<-has><has series#-><series#> - <-has><has year-><year> - <-has><title short><has narrator-> {<first narrator>}<-has>",
                file: "<title> [<asin>]",
                chapter_file: "<ch#> - <chapter title>",
            },
            // Matches Classic Libation Templates.*.DefaultTemplate
            // (Source/LibationFileManager/Templates/Templates.cs).
            Self::Classic => NamingProfileTemplates {
                folder: "<title short> [<id>]",
                file: "<title> [<id>]",
                chapter_file: "<title> [<id>] - <ch# 0> - <ch title>",
            },
        }
    }
}

/// Resolved folder / file / chapter-file templates after applying profile
/// defaults and optional per-field overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNamingTemplates {
    pub folder: String,
    pub file: String,
    pub chapter_file: String,
}

impl ResolvedNamingTemplates {
    /// Resolve templates: each explicit override wins over the profile default.
    #[must_use]
    pub fn resolve(
        profile: NamingProfile,
        folder: Option<&str>,
        file: Option<&str>,
        chapter_file: Option<&str>,
    ) -> Self {
        let defaults = profile.templates();
        Self {
            folder: folder.unwrap_or(defaults.folder).to_string(),
            file: file.unwrap_or(defaults.file).to_string(),
            chapter_file: chapter_file.unwrap_or(defaults.chapter_file).to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_audiobookshelf() {
        assert_eq!(NamingProfile::default(), NamingProfile::Audiobookshelf);
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(
            NamingProfile::parse("ABS"),
            Some(NamingProfile::Audiobookshelf)
        );
        assert_eq!(
            NamingProfile::parse("classic"),
            Some(NamingProfile::Classic)
        );
        assert_eq!(NamingProfile::parse("nope"), None);
    }

    #[test]
    fn overrides_win_per_field() {
        let resolved = ResolvedNamingTemplates::resolve(
            NamingProfile::Audiobookshelf,
            Some("<author>"),
            None,
            Some("<ch#>"),
        );
        assert_eq!(resolved.folder, "<author>");
        assert_eq!(resolved.file, "<title> [<asin>]");
        assert_eq!(resolved.chapter_file, "<ch#>");
    }

    #[test]
    fn classic_matches_libation_desktop_defaults() {
        let t = NamingProfile::Classic.templates();
        assert_eq!(t.folder, "<title short> [<id>]");
        assert_eq!(t.file, "<title> [<id>]");
        assert_eq!(t.chapter_file, "<title> [<id>] - <ch# 0> - <ch title>");
    }
}
