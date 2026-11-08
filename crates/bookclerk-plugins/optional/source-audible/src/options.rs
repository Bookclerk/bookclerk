//! Per-fetch download options: the host's wire fetch knobs plus the
//! instance's `bitrate` setting.

use std::path::PathBuf;

use audible_rs::downloader::Quality;
use bookclerk_plugin_sdk::FetchOptions;
use serde_json::Value;

/// Audible license bitrate tier, the instance's `bitrate` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bitrate {
    /// Highest bitrate Audible offers for the title (the default).
    #[default]
    High,
    /// Audible's standard, lower bitrate.
    Normal,
}

impl Bitrate {
    /// Reads the instance's `bitrate` setting (`high` / `normal`, any case).
    ///
    /// # Arguments
    ///
    /// * `source_config` - The instance's settings document; may be `null`.
    ///
    /// # Returns
    ///
    /// The configured tier, or [`Bitrate::High`] when the setting is missing
    /// or holds an unknown value.
    #[must_use]
    pub fn from_source_config(source_config: &Value) -> Self {
        source_config
            .get("bitrate")
            .and_then(Value::as_str)
            .and_then(|raw| match raw.trim().to_ascii_lowercase().as_str() {
                "high" => Some(Self::High),
                "normal" => Some(Self::Normal),
                _ => None,
            })
            .unwrap_or_default()
    }
}

impl From<Bitrate> for Quality {
    fn from(bitrate: Bitrate) -> Self {
        match bitrate {
            Bitrate::High => Self::High,
            Bitrate::Normal => Self::Normal,
        }
    }
}

/// What one `fetchTitle` call asks of the download path.
///
/// Packaging (format, naming, tagging) is the host's job after the guest
/// returns plain audio, so only the knobs that change what is downloaded
/// live here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOptions {
    /// License bitrate tier.
    pub quality: Bitrate,
    /// Prefer the Widevine/CENC download when Audible offers it.
    pub widevine: bool,
    /// Prefer xHE-AAC on the Widevine path when offered.
    pub xhe_aac: bool,
    /// Bring-your-own Widevine `.wvd` the host granted to the guest.
    pub widevine_cdm: Option<PathBuf>,
    /// Remote L3 CDM provider URL (`None` = classic Libation AudibleCdm;
    /// empty or `off` = disabled).
    pub widevine_cdm_provider: Option<String>,
    /// Cover image size to request (`500`, `1215` or `native`).
    pub cover_size: String,
    /// Chapter API layout to request (`tree` or `flat`).
    pub chapter_layout: String,
    /// Trim Audible's brand intro and outro from the remux window.
    pub strip_audible_brand_audio: bool,
    /// Download speed cap in KB/s (`0` = unlimited).
    pub download_speed_limit_kbps: u32,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            quality: Bitrate::default(),
            widevine: false,
            xhe_aac: false,
            widevine_cdm: None,
            widevine_cdm_provider: None,
            cover_size: String::from("500"),
            chapter_layout: String::from("tree"),
            strip_audible_brand_audio: false,
            download_speed_limit_kbps: 0,
        }
    }
}

impl DownloadOptions {
    /// Builds the options for one fetch from the host's wire knobs.
    ///
    /// Empty `cover_size` and `chapter_layout` keep the defaults (`500` and
    /// `tree`).
    ///
    /// # Arguments
    ///
    /// * `fetch` - `FetchTitleParams.fetch` as the host sent it.
    /// * `source_config` - The instance's settings document (for `bitrate`).
    #[must_use]
    pub fn from_host(fetch: &FetchOptions, source_config: &Value) -> Self {
        let defaults = Self::default();
        Self {
            quality: Bitrate::from_source_config(source_config),
            widevine: fetch.widevine,
            xhe_aac: fetch.xhe_aac,
            widevine_cdm: fetch.widevine_cdm_path.as_deref().map(PathBuf::from),
            widevine_cdm_provider: fetch.widevine_cdm_provider.clone(),
            cover_size: non_empty_or(&fetch.cover_size, defaults.cover_size),
            chapter_layout: non_empty_or(&fetch.chapter_layout, defaults.chapter_layout),
            strip_audible_brand_audio: fetch.strip_audible_brand_audio,
            download_speed_limit_kbps: fetch.download_speed_limit_kbps,
        }
    }
}

/// Returns `value` unless it is empty, in which case `fallback`.
fn non_empty_or(value: &str, fallback: String) -> String {
    if value.is_empty() {
        fallback
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bitrate_reads_the_setting_and_defaults_to_high() {
        assert_eq!(
            Bitrate::from_source_config(&json!({"bitrate": " Normal "})),
            Bitrate::Normal
        );
        assert_eq!(
            Bitrate::from_source_config(&json!({"bitrate": "high"})),
            Bitrate::High
        );
        assert_eq!(
            Bitrate::from_source_config(&json!({"bitrate": "lossless"})),
            Bitrate::High
        );
        assert_eq!(Bitrate::from_source_config(&Value::Null), Bitrate::High);
    }

    #[test]
    fn host_knobs_overlay_the_defaults() {
        let fetch = FetchOptions {
            widevine: true,
            xhe_aac: true,
            widevine_cdm_path: Some("/grant/device.wvd".into()),
            widevine_cdm_provider: Some("off".into()),
            cover_size: "1215".into(),
            chapter_layout: String::new(),
            strip_audible_brand_audio: true,
            download_speed_limit_kbps: 512,
            ..FetchOptions::default()
        };
        let options = DownloadOptions::from_host(&fetch, &json!({"bitrate": "normal"}));
        assert_eq!(
            options,
            DownloadOptions {
                quality: Bitrate::Normal,
                widevine: true,
                xhe_aac: true,
                widevine_cdm: Some(PathBuf::from("/grant/device.wvd")),
                widevine_cdm_provider: Some("off".into()),
                cover_size: "1215".into(),
                chapter_layout: "tree".into(),
                strip_audible_brand_audio: true,
                download_speed_limit_kbps: 512,
            }
        );
    }
}
