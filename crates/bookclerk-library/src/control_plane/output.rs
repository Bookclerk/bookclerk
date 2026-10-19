//! `core.output`: the shared acquire-packaging domain (formerly `[output]`).
//!
//! One cluster document holds format, Widevine, naming profile and templates,
//! sidecars, path sanitization, LAME knobs, and the multi-destination policy.
//! Per-destination roots, prefixes, and naming overrides stay on the `local` /
//! `s3` plugin instances. The host-local scratch directory
//! (`[output].in_progress`) is bootstrap and stays in `config.toml`: the
//! process needs it before the database opens, and it names a path on one
//! host's filesystem.
//!
//! The lifecycle matches `core.events`: the effective TOML and environment
//! values seed the document once, after which only a compare-and-swap write
//! changes it. See `docs/adr/control-plane.md`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use bookclerk_config::{
    BadBookAction, ChapterJsonMode, FileTimestampMode, LameConfig, MultiDestinationMode,
    NamingProfile, OutputConfig, OutputFormat, PathSanitizationMode, ReplacementRule,
};

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::{document, ConfigActor, ConfigurationDocument};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Namespace for the migrated shared `[output]` domain.
pub const OUTPUT_NAMESPACE: &str = "core.output";

/// Schema version of [`OutputSettingsV1`].
pub const OUTPUT_SCHEMA_VERSION: i64 = 1;

/// Maximum byte length of a naming template.
pub const MAX_OUTPUT_TEMPLATE_BYTES: usize = 1024;

/// Maximum number of explicit path replacement rules.
pub const MAX_OUTPUT_REPLACEMENT_RULES: usize = 128;

/// Maximum byte length of either side of one replacement rule.
const MAX_REPLACEMENT_SIDE_BYTES: usize = 64;

/// Maximum byte length of a path or provider string in the document.
const MAX_PATH_BYTES: usize = 1024;

/// Upper bound for `split_mp3_max_mb`.
const MAX_SPLIT_MP3_MB: u32 = 10 * 1024;

/// Upper bound for `minimum_file_duration_minutes` (one day).
const MAX_MINIMUM_FILE_DURATION_MINUTES: u32 = 24 * 60;

/// Upper bound for `max_filename_length` (bytes or UTF-16 units).
const MAX_FILENAME_LENGTH: u32 = 1024;

/// Upper bound for a numeric `cover_size`.
const MAX_COVER_SIZE: u32 = 10_000;

/// Every `output.*` key [`OutputSettingsV1::get`] and [`OutputSettingsV1::set`]
/// accept, in display order.
pub const OUTPUT_SETTING_KEYS: &[&str] = &[
    "output.format",
    "output.widevine",
    "output.xhe_aac",
    "output.widevine_cdm",
    "output.widevine_cdm_provider",
    "output.naming_profile",
    "output.folder_template",
    "output.file_template",
    "output.chapter_file_template",
    "output.chapter_title_template",
    "output.download_cover",
    "output.download_pdf",
    "output.create_cue",
    "output.fixup_metadata",
    "output.chapter_json",
    "output.save_metadata_json",
    "output.cover_size",
    "output.chapter_layout",
    "output.overwrite_existing",
    "output.multi_destination",
    "output.bad_book_action",
    "output.split_mp3_max_mb",
    "output.minimum_file_duration_minutes",
    "output.combine_nested_chapter_titles",
    "output.merge_opening_and_end_credits",
    "output.strip_unabridged",
    "output.strip_audible_brand_audio",
    "output.download_clips_bookmarks",
    "output.retain_aax_file",
    "output.download_speed_limit_kbps",
    "output.lame.target",
    "output.lame.vbr_quality",
    "output.lame.bitrate_kbps",
    "output.lame.mode",
    "output.lame.downsample_mono",
    "output.lame.constant_bitrate",
    "output.max_sample_rate",
    "output.creation_time",
    "output.last_write_time",
    "output.path_sanitization",
    "output.replacement_characters",
    "output.max_filename_length",
];

/// Write-only aliases [`OutputSettingsV1::set`] also accepts.
///
/// `output.save_chapter_json` takes a boolean or a chapter-JSON mode;
/// `output.split_files_by_chapter = true` selects the per-chapter MP3 format.
pub const OUTPUT_SETTING_ALIASES: &[&str] =
    &["output.save_chapter_json", "output.split_files_by_chapter"];

/// True when `key` is a `core.output` setting or alias.
#[must_use]
pub fn is_output_setting_key(key: &str) -> bool {
    OUTPUT_SETTING_KEYS.contains(&key) || OUTPUT_SETTING_ALIASES.contains(&key)
}

/// Typed `core.output` document, schema version 1.
///
/// Field meanings match [`OutputConfig`]. Unknown fields are rejected, inside
/// the nested `lame` and `replacement_characters` objects too, so a document
/// written by a newer binary under the same schema version fails closed
/// instead of being silently truncated on the next write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSettingsV1 {
    /// Post-acquire packaging format.
    pub format: OutputFormat,
    /// Prefer Widevine/CENC.
    pub widevine: bool,
    /// Prefer xHE-AAC on the Widevine path when offered.
    pub xhe_aac: bool,
    /// Optional `.wvd` path; relative paths resolve under each host's files dir.
    pub widevine_cdm: Option<PathBuf>,
    /// Remote L3 CDM provider (`None` = AudibleCdm, empty/`off` disables).
    pub widevine_cdm_provider: Option<String>,
    /// Named path-template preset.
    pub naming_profile: NamingProfile,
    /// Folder-path template override.
    pub folder_template: Option<String>,
    /// File-stem template override.
    pub file_template: Option<String>,
    /// Per-chapter file-stem template override.
    pub chapter_file_template: Option<String>,
    /// Embedded chapter-title template.
    pub chapter_title_template: Option<String>,
    /// Save cover JPEG alongside audio.
    pub download_cover: bool,
    /// Download companion PDF when available.
    pub download_pdf: bool,
    /// Write a `.cue` sidecar.
    pub create_cue: bool,
    /// Embed tags, cover, and chapters natively.
    pub fixup_metadata: bool,
    /// Chapter JSON sidecars.
    pub chapter_json: ChapterJsonMode,
    /// Persist raw catalog API JSON.
    pub save_metadata_json: bool,
    /// Cover image size (`500`, `1215`, another pixel size, or `native`).
    pub cover_size: String,
    /// Preferred chapter API layout (`tree`, `flat`, or `both`).
    pub chapter_layout: String,
    /// Re-acquire when media already exists at the destination.
    pub overwrite_existing: bool,
    /// Policy when only some enabled destinations have a title.
    pub multi_destination: MultiDestinationMode,
    /// Operator action when a title fails to acquire.
    pub bad_book_action: BadBookAction,
    /// Max MP3 part size in MiB for `split_mp3_by_size`.
    pub split_mp3_max_mb: u32,
    /// Drop chapter splits shorter than this many minutes.
    pub minimum_file_duration_minutes: u32,
    /// Flatten nested chapter titles.
    pub combine_nested_chapter_titles: bool,
    /// Merge opening/end credit chapters into neighbours.
    pub merge_opening_and_end_credits: bool,
    /// Strip an "Unabridged" suffix from titles used in naming.
    pub strip_unabridged: bool,
    /// Trim Audible brand intro/outro audio.
    pub strip_audible_brand_audio: bool,
    /// Download Audible clips/bookmarks sidecars.
    pub download_clips_bookmarks: bool,
    /// Keep the encrypted download.
    pub retain_aax_file: bool,
    /// Fetch speed cap in KB/s (`0` = unlimited).
    pub download_speed_limit_kbps: u32,
    /// LAME encoder knobs.
    #[serde(deserialize_with = "deserialize_lame")]
    pub lame: LameConfig,
    /// Optional output sample-rate ceiling in Hz.
    pub max_sample_rate: Option<u32>,
    /// File creation timestamp policy.
    pub creation_time: FileTimestampMode,
    /// File last-write timestamp policy.
    pub last_write_time: FileTimestampMode,
    /// Sanitization profile used when no explicit rules are set.
    pub path_sanitization: PathSanitizationMode,
    /// Explicit find/replace rules for path segments.
    #[serde(deserialize_with = "deserialize_replacement_rules")]
    pub replacement_characters: Vec<ReplacementRule>,
    /// Max length per path segment (`0` disables truncation).
    pub max_filename_length: u32,
}

/// Fails when `object` carries a key outside `known`.
///
/// # Errors
///
/// Returns a message naming the first unknown key under `what`.
fn reject_unknown_keys(
    object: &serde_json::Map<String, serde_json::Value>,
    known: &[&str],
    what: &str,
) -> std::result::Result<(), String> {
    match object.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(format!("unknown field `{key}` in {what}")),
        None => Ok(()),
    }
}

/// Deserializes [`OutputSettingsV1::lame`], rejecting keys [`LameConfig`]
/// does not define (it ignores them itself, for `config.toml`).
fn deserialize_lame<'de, D>(deserializer: D) -> std::result::Result<LameConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    if let serde_json::Value::Object(object) = &value {
        let defaults = serde_json::to_value(LameConfig::default()).map_err(D::Error::custom)?;
        let known: Vec<&str> = defaults
            .as_object()
            .map(|fields| fields.keys().map(String::as_str).collect())
            .unwrap_or_default();
        reject_unknown_keys(object, &known, "lame").map_err(D::Error::custom)?;
    }
    serde_json::from_value(value).map_err(D::Error::custom)
}

/// Deserializes [`OutputSettingsV1::replacement_characters`], rejecting
/// keys a [`ReplacementRule`] does not define.
fn deserialize_replacement_rules<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ReplacementRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    if let serde_json::Value::Array(rules) = &value {
        for rule in rules {
            if let serde_json::Value::Object(object) = rule {
                reject_unknown_keys(object, &["find", "replace"], "replacement_characters")
                    .map_err(D::Error::custom)?;
            }
        }
    }
    serde_json::from_value(value).map_err(D::Error::custom)
}

impl Default for OutputSettingsV1 {
    fn default() -> Self {
        Self::from_config_unchecked(&OutputConfig::default())
    }
}

/// Copies one bounded field from the second document onto the first.
type CopyField = fn(&mut OutputSettingsV1, &OutputSettingsV1);

/// Every field [`OutputSettingsV1::validate`] bounds, by `output.*` name,
/// for [`OutputSettingsV1::from_seed_lenient`].
const SEED_FIELDS: &[(&str, CopyField)] = &[
    ("widevine_cdm", |to, from| {
        to.widevine_cdm.clone_from(&from.widevine_cdm);
    }),
    ("widevine_cdm_provider", |to, from| {
        to.widevine_cdm_provider
            .clone_from(&from.widevine_cdm_provider);
    }),
    ("folder_template", |to, from| {
        to.folder_template.clone_from(&from.folder_template);
    }),
    ("file_template", |to, from| {
        to.file_template.clone_from(&from.file_template);
    }),
    ("chapter_file_template", |to, from| {
        to.chapter_file_template
            .clone_from(&from.chapter_file_template);
    }),
    ("chapter_title_template", |to, from| {
        to.chapter_title_template
            .clone_from(&from.chapter_title_template);
    }),
    ("cover_size", |to, from| {
        to.cover_size.clone_from(&from.cover_size);
    }),
    ("chapter_layout", |to, from| {
        to.chapter_layout.clone_from(&from.chapter_layout);
    }),
    ("split_mp3_max_mb", |to, from| {
        to.split_mp3_max_mb = from.split_mp3_max_mb;
    }),
    ("minimum_file_duration_minutes", |to, from| {
        to.minimum_file_duration_minutes = from.minimum_file_duration_minutes;
    }),
    ("max_filename_length", |to, from| {
        to.max_filename_length = from.max_filename_length;
    }),
    ("lame.target", |to, from| {
        to.lame.target.clone_from(&from.lame.target);
    }),
    ("lame.mode", |to, from| {
        to.lame.mode.clone_from(&from.lame.mode);
    }),
    ("lame.vbr_quality", |to, from| {
        to.lame.vbr_quality = from.lame.vbr_quality;
    }),
    ("lame.bitrate_kbps", |to, from| {
        to.lame.bitrate_kbps = from.lame.bitrate_kbps;
    }),
    ("max_sample_rate", |to, from| {
        to.max_sample_rate = from.max_sample_rate;
    }),
    ("replacement_characters", |to, from| {
        to.replacement_characters
            .clone_from(&from.replacement_characters);
    }),
];

impl OutputSettingsV1 {
    /// Copies the transitional `[output]` table, rejecting values the domain forbids.
    ///
    /// The deprecated `save_chapter_json` flag is folded into
    /// [`Self::chapter_json`], empty templates become unset, and the host-local
    /// `in_progress` directory is not part of the document.
    ///
    /// # Errors
    ///
    /// Returns an error when the table fails [`Self::validate`].
    pub fn from_config(config: &OutputConfig) -> Result<Self> {
        let settings = Self::from_config_unchecked(config);
        settings.validate()?;
        Ok(settings)
    }

    /// Builds the document from a seed `[output]` table (TOML / environment)
    /// for the first import, keeping every valid value and replacing each
    /// field that fails [`Self::validate`] with its default.
    ///
    /// Loading `config.toml` does not apply the document's bounds, so an
    /// install upgrading from file-based output settings can hold a value
    /// such as `max_filename_length = 2048` that ran before. Refusing it
    /// would stop the host from starting; the replaced fields are returned
    /// so the caller can log each one. Not used on a stored document, which
    /// is validated strictly.
    ///
    /// # Arguments
    ///
    /// * `config` - The seed `[output]` table.
    ///
    /// # Returns
    ///
    /// The document, and the names of the fields that were replaced.
    #[must_use]
    pub fn from_seed_lenient(config: &OutputConfig) -> (Self, Vec<&'static str>) {
        let seed = Self::from_config_unchecked(config);
        let defaults = Self::from_config_unchecked(&OutputConfig::default());
        let mut settings = seed.clone();
        let mut replaced = Vec::new();
        for (name, copy) in SEED_FIELDS {
            // Each field is judged alone: the defaults are valid, so a
            // candidate that differs from them only in this field fails
            // validation only for this field.
            let mut candidate = defaults.clone();
            copy(&mut candidate, &seed);
            if candidate.validate().is_err() {
                copy(&mut settings, &defaults);
                replaced.push(*name);
            }
        }
        (settings, replaced)
    }

    /// [`Self::from_config`] without validation.
    fn from_config_unchecked(config: &OutputConfig) -> Self {
        Self {
            format: config.format,
            widevine: config.widevine,
            xhe_aac: config.xhe_aac,
            widevine_cdm: config
                .widevine_cdm
                .clone()
                .filter(|path| !path.as_os_str().is_empty()),
            widevine_cdm_provider: config.widevine_cdm_provider.clone(),
            naming_profile: config.naming_profile,
            folder_template: non_empty(config.folder_template.as_deref()),
            file_template: non_empty(config.file_template.as_deref()),
            chapter_file_template: non_empty(config.chapter_file_template.as_deref()),
            chapter_title_template: non_empty(config.chapter_title_template.as_deref()),
            download_cover: config.download_cover,
            download_pdf: config.download_pdf,
            create_cue: config.create_cue,
            fixup_metadata: config.fixup_metadata,
            chapter_json: config.effective_chapter_json(),
            save_metadata_json: config.save_metadata_json,
            cover_size: config.cover_size.trim().to_ascii_lowercase(),
            chapter_layout: config.chapter_layout.trim().to_ascii_lowercase(),
            overwrite_existing: config.overwrite_existing,
            multi_destination: config.multi_destination,
            bad_book_action: config.bad_book_action,
            split_mp3_max_mb: config.split_mp3_max_mb,
            minimum_file_duration_minutes: config.minimum_file_duration_minutes,
            combine_nested_chapter_titles: config.combine_nested_chapter_titles,
            merge_opening_and_end_credits: config.merge_opening_and_end_credits,
            strip_unabridged: config.strip_unabridged,
            strip_audible_brand_audio: config.strip_audible_brand_audio,
            download_clips_bookmarks: config.download_clips_bookmarks,
            retain_aax_file: config.retain_aax_file,
            download_speed_limit_kbps: config.download_speed_limit_kbps,
            lame: config.lame.clone(),
            max_sample_rate: config.max_sample_rate,
            creation_time: config.creation_time,
            last_write_time: config.last_write_time,
            path_sanitization: config.path_sanitization,
            replacement_characters: config.replacement_characters.clone(),
            max_filename_length: config.max_filename_length,
        }
    }

    /// Writes the document onto the in-memory `[output]` table.
    ///
    /// `in_progress` is left as it was. A relative `widevine_cdm` resolves
    /// under `files_dir` when one is given, the same rule `config.toml` paths
    /// follow.
    pub fn apply_to(&self, config: &mut OutputConfig, files_dir: Option<&Path>) {
        config.format = self.format;
        config.widevine = self.widevine;
        config.xhe_aac = self.xhe_aac;
        config.widevine_cdm = self.widevine_cdm.as_ref().map(|path| match files_dir {
            Some(dir) if path.is_relative() => dir.join(path),
            _ => path.clone(),
        });
        config.widevine_cdm_provider = self.widevine_cdm_provider.clone();
        config.naming_profile = self.naming_profile;
        config.folder_template = self.folder_template.clone();
        config.file_template = self.file_template.clone();
        config.chapter_file_template = self.chapter_file_template.clone();
        config.chapter_title_template = self.chapter_title_template.clone();
        config.download_cover = self.download_cover;
        config.download_pdf = self.download_pdf;
        config.create_cue = self.create_cue;
        config.fixup_metadata = self.fixup_metadata;
        config.chapter_json = self.chapter_json;
        config.save_chapter_json = None;
        config.save_metadata_json = self.save_metadata_json;
        config.cover_size = self.cover_size.clone();
        config.chapter_layout = self.chapter_layout.clone();
        config.overwrite_existing = self.overwrite_existing;
        config.multi_destination = self.multi_destination;
        config.bad_book_action = self.bad_book_action;
        config.split_mp3_max_mb = self.split_mp3_max_mb;
        config.minimum_file_duration_minutes = self.minimum_file_duration_minutes;
        config.combine_nested_chapter_titles = self.combine_nested_chapter_titles;
        config.merge_opening_and_end_credits = self.merge_opening_and_end_credits;
        config.strip_unabridged = self.strip_unabridged;
        config.strip_audible_brand_audio = self.strip_audible_brand_audio;
        config.download_clips_bookmarks = self.download_clips_bookmarks;
        config.retain_aax_file = self.retain_aax_file;
        config.download_speed_limit_kbps = self.download_speed_limit_kbps;
        config.lame = self.lame.clone();
        config.max_sample_rate = self.max_sample_rate;
        config.creation_time = self.creation_time;
        config.last_write_time = self.last_write_time;
        config.path_sanitization = self.path_sanitization;
        config.replacement_characters = self.replacement_characters.clone();
        config.max_filename_length = self.max_filename_length;
    }

    /// Rejects values outside the domain bounds.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first field that is out of range.
    pub fn validate(&self) -> Result<()> {
        if let Some(path) = &self.widevine_cdm {
            let text = path.to_string_lossy();
            check_text("widevine_cdm", &text, MAX_PATH_BYTES)?;
            if text.trim().is_empty() {
                return Err(invalid("widevine_cdm must not be empty when set"));
            }
        }
        if let Some(provider) = &self.widevine_cdm_provider {
            check_text("widevine_cdm_provider", provider, MAX_PATH_BYTES)?;
        }
        for (name, template) in [
            ("folder_template", &self.folder_template),
            ("file_template", &self.file_template),
            ("chapter_file_template", &self.chapter_file_template),
            ("chapter_title_template", &self.chapter_title_template),
        ] {
            if let Some(template) = template {
                check_text(name, template, MAX_OUTPUT_TEMPLATE_BYTES)?;
                if template.trim().is_empty() {
                    return Err(invalid(&format!("{name} must not be empty when set")));
                }
            }
        }
        let cover = self.cover_size.as_str();
        let cover_ok = cover == "native"
            || cover
                .parse::<u32>()
                .is_ok_and(|n| (1..=MAX_COVER_SIZE).contains(&n));
        if !cover_ok {
            return Err(invalid(
                "cover_size must be `native` or a pixel size 1..=10000",
            ));
        }
        if !matches!(self.chapter_layout.as_str(), "tree" | "flat" | "both") {
            return Err(invalid("chapter_layout must be tree, flat, or both"));
        }
        if !(1..=MAX_SPLIT_MP3_MB).contains(&self.split_mp3_max_mb) {
            return Err(invalid("split_mp3_max_mb must be 1..=10240"));
        }
        if self.minimum_file_duration_minutes > MAX_MINIMUM_FILE_DURATION_MINUTES {
            return Err(invalid("minimum_file_duration_minutes must be 0..=1440"));
        }
        if self.max_filename_length > MAX_FILENAME_LENGTH {
            return Err(invalid("max_filename_length must be 0..=1024"));
        }
        check_word("lame.target", &self.lame.target)?;
        check_word("lame.mode", &self.lame.mode)?;
        if self.lame.vbr_quality > 9 {
            return Err(invalid("lame.vbr_quality must be 0..=9"));
        }
        if !(8..=320).contains(&self.lame.bitrate_kbps) {
            return Err(invalid("lame.bitrate_kbps must be 8..=320"));
        }
        if let Some(rate) = self.max_sample_rate {
            if !(8_000..=192_000).contains(&rate) {
                return Err(invalid("max_sample_rate must be 8000..=192000"));
            }
        }
        if self.replacement_characters.len() > MAX_OUTPUT_REPLACEMENT_RULES {
            return Err(invalid("replacement_characters allows at most 128 rules"));
        }
        for rule in &self.replacement_characters {
            if rule.find.is_empty() {
                return Err(invalid("replacement_characters find must not be empty"));
            }
            if rule.find.len() > MAX_REPLACEMENT_SIDE_BYTES
                || rule.replace.len() > MAX_REPLACEMENT_SIDE_BYTES
            {
                return Err(invalid(
                    "replacement_characters find/replace must be at most 64 bytes",
                ));
            }
            if rule.replace.contains(['/', '\\', '\0']) {
                return Err(invalid(
                    "replacement_characters replace must not contain a path separator",
                ));
            }
        }
        Ok(())
    }

    /// Formats one `output.*` key for display.
    ///
    /// Enums print their stored (snake_case) name; an unset optional value
    /// prints as an empty string. A disabled `widevine_cdm_provider` (stored
    /// empty) prints as `off`, which [`Self::set`] reads back as disabled.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "output.format" => enum_name(&self.format),
            "output.widevine" => self.widevine.to_string(),
            "output.xhe_aac" => self.xhe_aac.to_string(),
            "output.widevine_cdm" => self
                .widevine_cdm
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            // `None` (the AudibleCdm default) prints empty; a disabled
            // provider prints `off`, so `set` of the printed value keeps it
            // disabled instead of clearing it back to the default.
            "output.widevine_cdm_provider" => match self.widevine_cdm_provider.as_deref() {
                None => String::new(),
                Some(provider) if provider.trim().is_empty() => "off".into(),
                Some(provider) => provider.to_string(),
            },
            "output.naming_profile" => self.naming_profile.as_str().to_string(),
            "output.folder_template" => self.folder_template.clone().unwrap_or_default(),
            "output.file_template" => self.file_template.clone().unwrap_or_default(),
            "output.chapter_file_template" => {
                self.chapter_file_template.clone().unwrap_or_default()
            }
            "output.chapter_title_template" => {
                self.chapter_title_template.clone().unwrap_or_default()
            }
            "output.download_cover" => self.download_cover.to_string(),
            "output.download_pdf" => self.download_pdf.to_string(),
            "output.create_cue" => self.create_cue.to_string(),
            "output.fixup_metadata" => self.fixup_metadata.to_string(),
            "output.chapter_json" => enum_name(&self.chapter_json),
            "output.save_chapter_json" => self.chapter_json.wants_any().to_string(),
            "output.save_metadata_json" => self.save_metadata_json.to_string(),
            "output.cover_size" => self.cover_size.clone(),
            "output.chapter_layout" => self.chapter_layout.clone(),
            "output.overwrite_existing" => self.overwrite_existing.to_string(),
            "output.multi_destination" => enum_name(&self.multi_destination),
            "output.bad_book_action" => enum_name(&self.bad_book_action),
            "output.split_mp3_max_mb" => self.split_mp3_max_mb.to_string(),
            "output.minimum_file_duration_minutes" => {
                self.minimum_file_duration_minutes.to_string()
            }
            "output.combine_nested_chapter_titles" => {
                self.combine_nested_chapter_titles.to_string()
            }
            "output.merge_opening_and_end_credits" => {
                self.merge_opening_and_end_credits.to_string()
            }
            "output.strip_unabridged" => self.strip_unabridged.to_string(),
            "output.strip_audible_brand_audio" => self.strip_audible_brand_audio.to_string(),
            "output.download_clips_bookmarks" => self.download_clips_bookmarks.to_string(),
            "output.retain_aax_file" => self.retain_aax_file.to_string(),
            "output.download_speed_limit_kbps" => self.download_speed_limit_kbps.to_string(),
            "output.lame.target" => self.lame.target.clone(),
            "output.lame.vbr_quality" => self.lame.vbr_quality.to_string(),
            "output.lame.bitrate_kbps" => self.lame.bitrate_kbps.to_string(),
            "output.lame.mode" => self.lame.mode.clone(),
            "output.lame.downsample_mono" => self.lame.downsample_mono.to_string(),
            "output.lame.constant_bitrate" => self.lame.constant_bitrate.to_string(),
            "output.max_sample_rate" => self
                .max_sample_rate
                .map(|n| n.to_string())
                .unwrap_or_default(),
            "output.creation_time" => enum_name(&self.creation_time),
            "output.last_write_time" => enum_name(&self.last_write_time),
            "output.path_sanitization" => enum_name(&self.path_sanitization),
            "output.replacement_characters" => {
                serde_json::to_string(&self.replacement_characters).unwrap_or_default()
            }
            "output.max_filename_length" => self.max_filename_length.to_string(),
            _ => return None,
        })
    }

    /// Assigns one `output.*` key from its string form, then validates.
    ///
    /// Unlike `-o key=value` overrides, an unparseable value is an error,
    /// not a silent default. An empty value clears an optional field.
    /// `replacement_characters` takes a JSON array of `{find, replace}`.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown key, an unparseable value, or a value
    /// the domain rejects. `self` may be partially changed on error; callers
    /// edit a clone.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let v = value.trim();
        match key {
            "output.format" => {
                // The classic `DecryptToLossy` alias maps here and takes a
                // boolean: true re-encodes to MP3, false keeps the M4B.
                self.format = match OutputFormat::parse(v) {
                    Some(format) => format,
                    None if v.eq_ignore_ascii_case("mp3") || v.eq_ignore_ascii_case("lossy") => {
                        OutputFormat::SingleMp3
                    }
                    None => match bool_value(key, v) {
                        Ok(true) => OutputFormat::SingleMp3,
                        Ok(false) => OutputFormat::EnrichedM4b,
                        Err(_) => return Err(invalid("output.format is not a known format")),
                    },
                };
            }
            "output.widevine" => self.widevine = bool_value(key, v)?,
            "output.xhe_aac" => self.xhe_aac = bool_value(key, v)?,
            "output.widevine_cdm" => {
                self.widevine_cdm = (!v.is_empty()).then(|| PathBuf::from(v));
            }
            "output.widevine_cdm_provider" => {
                self.widevine_cdm_provider = (!v.is_empty()).then(|| v.to_string());
            }
            "output.naming_profile" => {
                self.naming_profile = NamingProfile::parse(v).ok_or_else(|| {
                    invalid("output.naming_profile must be audiobookshelf or classic")
                })?;
            }
            "output.folder_template" => self.folder_template = non_empty(Some(value)),
            "output.file_template" => self.file_template = non_empty(Some(value)),
            "output.chapter_file_template" => self.chapter_file_template = non_empty(Some(value)),
            "output.chapter_title_template" => {
                self.chapter_title_template = non_empty(Some(value));
            }
            "output.download_cover" => self.download_cover = bool_value(key, v)?,
            "output.download_pdf" => self.download_pdf = bool_value(key, v)?,
            "output.create_cue" => self.create_cue = bool_value(key, v)?,
            "output.fixup_metadata" => self.fixup_metadata = bool_value(key, v)?,
            "output.chapter_json" | "output.save_chapter_json" => {
                self.chapter_json = ChapterJsonMode::parse(v).ok_or_else(|| {
                    invalid("output.chapter_json must be off, flat, tree, both, or a boolean")
                })?;
            }
            "output.save_metadata_json" => self.save_metadata_json = bool_value(key, v)?,
            "output.cover_size" => self.cover_size = v.to_ascii_lowercase(),
            "output.chapter_layout" => self.chapter_layout = v.to_ascii_lowercase(),
            "output.overwrite_existing" => self.overwrite_existing = bool_value(key, v)?,
            "output.multi_destination" => {
                self.multi_destination = match v.to_ascii_lowercase().replace('-', "_").as_str() {
                    "sync_missing" => MultiDestinationMode::SyncMissing,
                    "refetch_missing" => MultiDestinationMode::RefetchMissing,
                    "refetch_all" => MultiDestinationMode::RefetchAll,
                    _ => {
                        return Err(invalid(
                            "output.multi_destination must be sync_missing, refetch_missing, \
                             or refetch_all",
                        ))
                    }
                };
            }
            "output.bad_book_action" => {
                self.bad_book_action = match v.to_ascii_lowercase().as_str() {
                    "ask" => BadBookAction::Ask,
                    "abort" => BadBookAction::Abort,
                    "retry" => BadBookAction::Retry,
                    "ignore" => BadBookAction::Ignore,
                    _ => {
                        return Err(invalid(
                            "output.bad_book_action must be ask, abort, retry, or ignore",
                        ))
                    }
                };
            }
            "output.split_files_by_chapter" => {
                if bool_value(key, v)? {
                    self.format = OutputFormat::SplitMp3ByChapter;
                } else if self.format == OutputFormat::SplitMp3ByChapter {
                    self.format = OutputFormat::SingleMp3;
                }
            }
            "output.split_mp3_max_mb" => self.split_mp3_max_mb = number(key, v)?,
            "output.minimum_file_duration_minutes" => {
                self.minimum_file_duration_minutes = number(key, v)?;
            }
            "output.combine_nested_chapter_titles" => {
                self.combine_nested_chapter_titles = bool_value(key, v)?;
            }
            "output.merge_opening_and_end_credits" => {
                self.merge_opening_and_end_credits = bool_value(key, v)?;
            }
            "output.strip_unabridged" => self.strip_unabridged = bool_value(key, v)?,
            "output.strip_audible_brand_audio" => {
                self.strip_audible_brand_audio = bool_value(key, v)?;
            }
            "output.download_clips_bookmarks" => {
                self.download_clips_bookmarks = bool_value(key, v)?;
            }
            "output.retain_aax_file" => self.retain_aax_file = bool_value(key, v)?,
            "output.download_speed_limit_kbps" => {
                self.download_speed_limit_kbps = number(key, v)?;
            }
            "output.lame.target" => self.lame.target = v.to_ascii_lowercase(),
            "output.lame.vbr_quality" => self.lame.vbr_quality = number(key, v)?,
            "output.lame.bitrate_kbps" => self.lame.bitrate_kbps = number(key, v)?,
            "output.lame.mode" => self.lame.mode = v.to_ascii_lowercase(),
            "output.lame.downsample_mono" => self.lame.downsample_mono = bool_value(key, v)?,
            "output.lame.constant_bitrate" => self.lame.constant_bitrate = bool_value(key, v)?,
            "output.max_sample_rate" => {
                self.max_sample_rate = if v.is_empty() {
                    None
                } else {
                    Some(number(key, v)?)
                };
            }
            "output.creation_time" => self.creation_time = timestamp_mode(key, v)?,
            "output.last_write_time" => self.last_write_time = timestamp_mode(key, v)?,
            "output.path_sanitization" => {
                self.path_sanitization =
                    match v.to_ascii_lowercase().as_str() {
                        "auto" => PathSanitizationMode::Auto,
                        "windows" => PathSanitizationMode::Windows,
                        "posix" => PathSanitizationMode::Posix,
                        "s3" => PathSanitizationMode::S3,
                        "none" => PathSanitizationMode::None,
                        _ => return Err(invalid(
                            "output.path_sanitization must be auto, windows, posix, s3, or none",
                        )),
                    };
            }
            "output.replacement_characters" => {
                self.replacement_characters = if v.is_empty() {
                    Vec::new()
                } else {
                    serde_json::from_str(v).map_err(|_| {
                        invalid(
                            "output.replacement_characters must be a JSON array of \
                             {\"find\", \"replace\"} objects",
                        )
                    })?
                };
            }
            "output.max_filename_length" => self.max_filename_length = number(key, v)?,
            other => {
                return Err(invalid(&format!("unknown output setting: {other}")));
            }
        }
        self.validate()
    }
}

/// Copies the authoritative output document onto `config` for `cluster_id`.
///
/// Revisions are only comparable within one cluster. Callers that switch
/// databases pass the new cluster id explicitly.
pub fn overlay_output(
    config: &mut bookclerk_config::Config,
    output: &ConfigurationDocument<OutputSettingsV1>,
    cluster_id: &str,
) {
    let files_dir = config.paths.as_ref().map(|paths| paths.files_dir.clone());
    output
        .body
        .apply_to(&mut config.output, files_dir.as_deref());
    config.output_revision = Some(output.revision);
    config.output_authority = Some(cluster_id.to_string());
}

/// Loads the cluster output document.
///
/// # Errors
///
/// Returns an error when the document is missing, unsupported, or invalid.
pub async fn load_output(store: &LibraryStore) -> Result<ConfigurationDocument<OutputSettingsV1>> {
    let stored = load_document(store, &DocumentKey::cluster(OUTPUT_NAMESPACE))
        .await?
        .ok_or_else(|| {
            LibraryError::NotFound("configuration core.output is not initialized".into())
        })?;
    parse_output(stored)
}

/// Copies `[output]` with a `widevine_cdm` under `files_dir` made relative again.
///
/// `Config::load` resolves a relative CDM path under the loading host's files
/// directory. The shared document keeps it relative, so every host rebases it
/// under its own `BOOKCLERK_FILES_DIR` in [`OutputSettingsV1::apply_to`]. A
/// path outside `files_dir` (or equal to it) is kept as written.
///
/// # Arguments
///
/// * `seed` - The loaded `[output]` table.
/// * `files_dir` - The loading host's files directory.
#[must_use]
pub fn portable_output_seed(seed: &OutputConfig, files_dir: &Path) -> OutputConfig {
    let mut portable = seed.clone();
    if let Some(cdm) = &seed.widevine_cdm {
        if let Ok(relative) = cdm.strip_prefix(files_dir) {
            if !relative.as_os_str().is_empty() {
                portable.widevine_cdm = Some(relative.to_path_buf());
            }
        }
    }
    portable
}

/// Inserts `[output]` when the cluster document is absent.
///
/// An existing document is parsed and returned without reading the seed, so
/// an obsolete or out-of-range TOML/environment value cannot fail startup or
/// replace the stored document.
///
/// # Errors
///
/// Returns an error when the stored document is invalid, or when the document
/// is absent and the actor may not import. An absent document is seeded with
/// [`OutputSettingsV1::from_seed_lenient`]: an out-of-range seed field is
/// logged and replaced by its default rather than failing startup.
pub async fn import_output_if_absent(
    store: &LibraryStore,
    actor: &ConfigActor,
    seed: &OutputConfig,
    operation_id: &str,
) -> Result<ConfigurationDocument<OutputSettingsV1>> {
    let key = DocumentKey::cluster(OUTPUT_NAMESPACE);
    if let Some(existing) = load_document(store, &key).await? {
        return parse_output(existing);
    }
    let (body, replaced) = OutputSettingsV1::from_seed_lenient(seed);
    for field in replaced {
        tracing::warn!(
            field = %format_args!("output.{field}"),
            "the [output] value is out of range; core.output starts with its default instead"
        );
    }
    let json = to_json(&body)?;
    let stored = import_if_absent(
        store,
        actor,
        &key,
        OUTPUT_SCHEMA_VERSION,
        &json,
        operation_id,
    )
    .await?;
    parse_output(stored)
}

/// Replaces the cluster output document.
///
/// # Errors
///
/// Returns an error when validation, authorization, or the compare-and-swap fails.
pub async fn replace_output(
    store: &LibraryStore,
    actor: &ConfigActor,
    expected_revision: i64,
    body: &OutputSettingsV1,
    operation_id: &str,
) -> Result<OutputReplace> {
    body.validate()?;
    let json = to_json(body)?;
    match replace_document(
        store,
        actor,
        &DocumentKey::cluster(OUTPUT_NAMESPACE),
        OUTPUT_SCHEMA_VERSION,
        expected_revision,
        &json,
        operation_id,
    )
    .await?
    {
        ReplaceOutcome::Applied(stored) => {
            Ok(OutputReplace::Applied(Box::new(parse_output(stored)?)))
        }
        ReplaceOutcome::Conflict { current_revision } => {
            Ok(OutputReplace::Conflict { current_revision })
        }
        ReplaceOutcome::Replayed { revision } => Ok(OutputReplace::Replayed { revision }),
    }
}

/// Outcome of [`replace_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputReplace {
    /// The new document is committed.
    Applied(Box<ConfigurationDocument<OutputSettingsV1>>),
    /// `expected_revision` was not current. The stored document is unchanged.
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
    /// This operation id already committed at `revision`.
    Replayed {
        /// Revision of the original commit.
        revision: i64,
    },
}

/// Parses a stored `core.output` row, failing closed on an unknown schema.
pub(super) fn parse_output(
    stored: StoredDocument,
) -> Result<ConfigurationDocument<OutputSettingsV1>> {
    if stored.schema_version != OUTPUT_SCHEMA_VERSION {
        return Err(documents::unsupported(
            OUTPUT_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: OutputSettingsV1 = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
    body.validate()?;
    Ok(document(stored, body))
}

/// Serializes a body for storage.
fn to_json(body: &OutputSettingsV1) -> Result<String> {
    serde_json::to_string(body)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))
}

/// `Some(trimmed-owned)` for a non-blank string, else `None`.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

/// Rejects control characters and oversize strings.
fn check_text(name: &str, value: &str, max: usize) -> Result<()> {
    if value.len() > max {
        return Err(invalid(&format!("{name} must be at most {max} bytes")));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(&format!(
            "{name} must not contain control characters"
        )));
    }
    Ok(())
}

/// Accepts a short ASCII word (`quality`, `joint_stereo`, …).
fn check_word(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(invalid(&format!(
            "{name} must be 1..=32 ASCII letters, digits, `_`, or `-`"
        )));
    }
    Ok(())
}

/// Strict boolean parse for [`OutputSettingsV1::set`].
fn bool_value(key: &str, value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(invalid(&format!("{key} must be a boolean value"))),
    }
}

/// Strict unsigned parse for [`OutputSettingsV1::set`].
fn number<T: std::str::FromStr>(key: &str, value: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| invalid(&format!("{key} must be a non-negative integer")))
}

/// Strict timestamp-mode parse (classic names accepted).
fn timestamp_mode(key: &str, value: &str) -> Result<FileTimestampMode> {
    match value.to_ascii_lowercase().as_str() {
        "now" => Ok(FileTimestampMode::Now),
        "purchased" | "dateadded" => Ok(FileTimestampMode::Purchased),
        "published" | "releasedate" => Ok(FileTimestampMode::Published),
        _ => Err(invalid(&format!(
            "{key} must be now, purchased, or published"
        ))),
    }
}

/// Serde name of a unit enum variant.
fn enum_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::new(),
    }
}

/// Error for an output value outside the domain bounds.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid configuration: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_document_validates_and_round_trips() {
        let body = OutputSettingsV1::default();
        body.validate().expect("defaults are valid");
        let json = serde_json::to_string(&body).unwrap();
        let back: OutputSettingsV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(back, body);
        assert!(json.len() < super::super::MAX_CONFIGURATION_DOCUMENT_BYTES);
    }

    #[test]
    fn unknown_fields_fail_closed() {
        let mut value = serde_json::to_value(OutputSettingsV1::default()).unwrap();
        value["in_progress"] = serde_json::json!("/scratch");
        assert!(serde_json::from_value::<OutputSettingsV1>(value).is_err());
    }

    #[test]
    fn unknown_nested_fields_fail_closed() {
        let base = serde_json::to_value(OutputSettingsV1::default()).unwrap();
        let mut lame = base.clone();
        lame["lame"]["future_knob"] = serde_json::json!(1);
        let err = serde_json::from_value::<OutputSettingsV1>(lame).unwrap_err();
        assert!(err.to_string().contains("future_knob"), "{err}");

        let mut rules = base.clone();
        rules["replacement_characters"] =
            serde_json::json!([{ "find": ":", "replace": "_", "regex": true }]);
        let err = serde_json::from_value::<OutputSettingsV1>(rules).unwrap_err();
        assert!(err.to_string().contains("regex"), "{err}");

        // A partial `lame` object still takes defaults for the rest.
        let mut partial = base;
        partial["lame"] = serde_json::json!({ "vbr_quality": 4 });
        let back = serde_json::from_value::<OutputSettingsV1>(partial).unwrap();
        assert_eq!(back.lame.vbr_quality, 4);
        assert_eq!(back.lame.target, LameConfig::default().target);
    }

    #[test]
    fn from_config_folds_deprecated_and_blank_fields() {
        let config = OutputConfig {
            save_chapter_json: Some(true),
            chapter_layout: "Flat".into(),
            folder_template: Some("   ".into()),
            in_progress: Some(PathBuf::from("/scratch")),
            ..OutputConfig::default()
        };
        let body = OutputSettingsV1::from_config(&config).unwrap();
        assert_eq!(body.chapter_json, ChapterJsonMode::Flat);
        assert_eq!(body.chapter_layout, "flat");
        assert_eq!(body.folder_template, None);

        let mut applied = OutputConfig {
            in_progress: Some(PathBuf::from("/host-local")),
            ..OutputConfig::default()
        };
        body.apply_to(&mut applied, None);
        assert_eq!(applied.in_progress, Some(PathBuf::from("/host-local")));
        assert_eq!(applied.save_chapter_json, None);
        assert_eq!(applied.chapter_json, ChapterJsonMode::Flat);
    }

    #[test]
    fn portable_seed_keeps_files_dir_cdm_paths_relative() {
        let files_dir = Path::new("/srv/host-a");
        let mut seed = OutputConfig {
            widevine_cdm: Some(files_dir.join("cdm/widevine.wvd")),
            ..OutputConfig::default()
        };
        let portable = portable_output_seed(&seed, files_dir);
        assert_eq!(
            portable.widevine_cdm.as_deref(),
            Some(Path::new("cdm/widevine.wvd"))
        );
        let body = OutputSettingsV1::from_config(&portable).unwrap();
        let mut host_b = OutputConfig::default();
        body.apply_to(&mut host_b, Some(Path::new("/srv/host-b")));
        assert_eq!(
            host_b.widevine_cdm.as_deref(),
            Some(Path::new("/srv/host-b/cdm/widevine.wvd"))
        );
        // A path outside the files directory, or the directory itself, stays.
        for kept in ["/opt/cdm/widevine.wvd", "/srv/host-a"] {
            seed.widevine_cdm = Some(PathBuf::from(kept));
            let portable = portable_output_seed(&seed, files_dir);
            assert_eq!(portable.widevine_cdm.as_deref(), Some(Path::new(kept)));
        }
    }

    #[test]
    fn format_accepts_the_classic_lossy_booleans() {
        let mut body = OutputSettingsV1::default();
        for (value, format) in [
            ("true", OutputFormat::SingleMp3),
            ("false", OutputFormat::EnrichedM4b),
            ("mp3", OutputFormat::SingleMp3),
            ("lossy", OutputFormat::SingleMp3),
        ] {
            body.set("output.format", value).unwrap();
            assert_eq!(body.format, format, "{value}");
        }
        assert!(body.set("output.format", "wav-ish").is_err());
    }

    #[test]
    fn invalid_seed_is_rejected() {
        for bad in [
            OutputConfig {
                cover_size: "huge".into(),
                ..OutputConfig::default()
            },
            OutputConfig {
                split_mp3_max_mb: 0,
                ..OutputConfig::default()
            },
            OutputConfig {
                chapter_layout: "spiral".into(),
                ..OutputConfig::default()
            },
            OutputConfig {
                folder_template: Some("a\u{7}b".into()),
                ..OutputConfig::default()
            },
            OutputConfig {
                replacement_characters: vec![ReplacementRule {
                    find: ":".into(),
                    replace: "/".into(),
                }],
                ..OutputConfig::default()
            },
        ] {
            assert!(OutputSettingsV1::from_config(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn set_is_strict_and_get_round_trips() {
        let mut body = OutputSettingsV1::default();
        body.set("output.widevine", "yes").unwrap();
        assert!(body.widevine);
        assert!(body.clone().set("output.widevine", "maybe").is_err());
        body.set("output.format", "mp3").unwrap();
        assert_eq!(body.get("output.format").as_deref(), Some("single_mp3"));
        body.set("output.multi_destination", "refetch-all").unwrap();
        assert_eq!(
            body.get("output.multi_destination").as_deref(),
            Some("refetch_all")
        );
        body.set("output.bad_book_action", "Abort").unwrap();
        assert_eq!(body.get("output.bad_book_action").as_deref(), Some("abort"));
        body.set("output.folder_template", "<author>").unwrap();
        body.set("output.folder_template", "").unwrap();
        assert_eq!(body.folder_template, None);
        body.set(
            "output.replacement_characters",
            r#"[{"find":":","replace":"-"}]"#,
        )
        .unwrap();
        assert_eq!(body.replacement_characters.len(), 1);
        assert!(body.clone().set("output.split_mp3_max_mb", "0").is_err());
        assert!(body.clone().set("output.in_progress", "/x").is_err());
        for key in OUTPUT_SETTING_KEYS {
            let value = body.get(key).expect(key);
            let mut copy = body.clone();
            copy.set(key, &value)
                .unwrap_or_else(|err| panic!("{key}: {err}"));
            assert_eq!(copy, body, "{key} round trip");
        }
    }

    #[test]
    fn disabled_cdm_provider_survives_get_set_round_trip() {
        const KEY: &str = "output.widevine_cdm_provider";
        // A TOML `widevine_cdm_provider = ""` seed disables the remote CDM.
        let disabled = OutputSettingsV1::from_config(&OutputConfig {
            widevine_cdm_provider: Some(String::new()),
            ..OutputConfig::default()
        })
        .unwrap();
        let shown = disabled.get(KEY).unwrap();
        assert_eq!(shown, "off");
        let mut copy = disabled.clone();
        copy.set(KEY, &shown).unwrap();
        assert_eq!(copy.widevine_cdm_provider.as_deref(), Some("off"));

        // The default (AudibleCdm) still prints empty and stays the default.
        let default = OutputSettingsV1::default();
        assert_eq!(default.widevine_cdm_provider, None);
        let mut copy = default.clone();
        copy.set(KEY, &default.get(KEY).unwrap()).unwrap();
        assert_eq!(copy.widevine_cdm_provider, None);
    }

    #[test]
    fn apply_resolves_relative_cdm_under_files_dir() {
        let body = OutputSettingsV1 {
            widevine_cdm: Some(PathBuf::from("widevine.wvd")),
            ..OutputSettingsV1::default()
        };
        let mut config = OutputConfig::default();
        body.apply_to(&mut config, Some(Path::new("/files")));
        assert_eq!(
            config.widevine_cdm,
            Some(PathBuf::from("/files/widevine.wvd"))
        );
    }
}
