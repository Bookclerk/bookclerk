//! Named path-template profiles for acquired audiobook layouts.
//!
//! Profiles are presets for `folder_template` / `file_template` /
//! `chapter_file_template`. Explicit template overrides in config still win
//! per field. New consumer layouts can be added here without changing the
//! naming engine.

use serde::{Deserialize, Serialize};

/// Preset path-template profile.
///
/// Select via `output.naming_profile`. Individual `folder_template` /
/// `file_template` / `chapter_file_template` values override the profile when
/// set.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[schema(example = "audiobookshelf")]
pub enum NamingProfile {
    /// [Audiobookshelf](https://audiobookshelf.org/docs/documentation/libraries/book-library/directory-structure/)
    /// layout: `{Author}/{Series}/{N} - {Book}` or `{Author}/{Book}`; the
    /// file is the full title with the edition's library id.
    #[default]
    Audiobookshelf,
    /// Previous Libation desktop defaults (`Templates.*.DefaultTemplate`).
    Classic,
}

/// Concrete templates contributed by a [`NamingProfile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamingProfileTemplates {
    /// Default folder-path template for this profile (Libation tag syntax).
    pub folder: &'static str,
    /// Default file-stem template for this profile (no extension).
    pub file: &'static str,
    /// Default per-chapter file-stem template when splitting by chapter.
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
                "Audiobookshelf Author/{Series/}{N - }Short Title/Title [library id]"
            }
            Self::Classic => "Classic Libation <title short> [<id>]/<title> [<id>]",
        }
    }

    /// Parse a profile id (case-insensitive). Accepts common aliases.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "audiobookshelf" | "abs" | "bookshelf" => Some(Self::Audiobookshelf),
            "classic" | "default-classic" => Some(Self::Classic),
            _ => None,
        }
    }

    /// Built-in folder / file / chapter-file templates for this profile.
    #[must_use]
    pub fn templates(self) -> NamingProfileTemplates {
        match self {
            // Author/{Series/}{N - }Short Title/Full Title [library id]
            // Matches Audiobookshelf Author/Series/Book (or Author/Book)
            // scanning; Audiobookshelf reads the sequence only before ` - `.
            // The library id stays out of the folder, where Audiobookshelf
            // would read a bracketed value as an ASIN.
            Self::Audiobookshelf => NamingProfileTemplates {
                folder: "<first author>/<has series-><first series>/<-has><has series#-><series#> - <-has><title short>",
                file: "<title> [<library id>]",
                chapter_file: "<ch#> - <chapter title> [<library id>]",
            },
            // Classic Libation Templates.*.DefaultTemplate, with the edition's
            // library id added to the folder so editions never share a path
            // (Source/LibationFileManager/Templates/Templates.cs).
            Self::Classic => NamingProfileTemplates {
                folder: "<title short> [<id>] [<library id>]",
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
    /// Folder-path template after profile defaults and optional overrides.
    pub folder: String,
    /// File-stem template after profile defaults and optional overrides.
    pub file: String,
    /// Chapter file-stem template after profile defaults and optional overrides.
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
        assert_eq!(resolved.file, "<title> [<library id>]");
        assert_eq!(resolved.chapter_file, "<ch#>");
    }

    #[test]
    fn classic_matches_bookclerk_desktop_defaults_plus_the_library_id() {
        let t = NamingProfile::Classic.templates();
        assert_eq!(t.folder, "<title short> [<id>] [<library id>]");
        assert_eq!(t.file, "<title> [<id>]");
        assert_eq!(t.chapter_file, "<title> [<id>] - <ch# 0> - <ch title>");
    }
}
