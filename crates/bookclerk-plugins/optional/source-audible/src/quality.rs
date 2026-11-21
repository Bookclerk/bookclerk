//! `qualityOffers`: which stream an account would download for a title
//! today, read from Audible's content metadata endpoint without licensing
//! anything.
//!
//! The request asks for the same DRM path and bitrate tier a download
//! would (`DownloadOptions`), and the answer's `content_reference` names
//! the codec and a `<FORMAT>_<kHz>_<kbps>` content format (`AAX_44_128`,
//! `XHE_44_137`). Spatial codecs (Dolby E-AC-3 and AC-4) are never offered:
//! Bookclerk never asks Audible for spatial audio, so a title whose
//! reference names one is answered "not known". So is an xHE-AAC master
//! when the download would not ask for xHE-AAC.

use audible_rs::api::client::Client;
use audible_rs::downloader::{self, Quality};
use bookclerk_plugin_sdk::{
    OfferedAacProfile, OfferedCodec, QualityOffer, QualityOffersParams, QualityOffersResult,
    TitleQualityOffer,
};
use serde_json::Value;

use crate::error::{AudibleError, Result};
use crate::options::DownloadOptions;

/// The offer a title's `content_reference` names, for a download that asks
/// for xHE-AAC when `xhe` is set.
///
/// # Returns
///
/// `None` when the reference names no codec this guest recognises, a
/// spatial codec, or xHE-AAC that the download would not ask for.
#[must_use]
pub fn offer_from_reference(reference: &Value, xhe: bool) -> Option<QualityOffer> {
    let codec = reference
        .get("codec")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let format = reference
        .get("content_format")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let mut parts = format.split('_');
    let family = parts.next().unwrap_or("");
    let sample_rate_hz = parts.next().and_then(sample_rate_of);
    let bitrate_kbps = parts
        .next()
        .and_then(|kbps| kbps.parse::<u32>().ok())
        .filter(|kbps| (1..=1_000).contains(kbps));
    let spatial = codec.starts_with("ec") || codec.starts_with("ac-4") || codec.starts_with("ac4");
    let spatial = spatial || matches!(family, "EC3" | "EAC3" | "AC4" | "ATMOS");
    if spatial {
        return None;
    }
    let (codec, aac_profile) = if codec.starts_with("mp4a.40.42") || family == "XHE" {
        if !xhe {
            return None;
        }
        (OfferedCodec::Aac, OfferedAacProfile::Xhe)
    } else if codec.starts_with("mp4a.40.29") {
        (OfferedCodec::Aac, OfferedAacProfile::HeV2)
    } else if codec.starts_with("mp4a.40.5") {
        (OfferedCodec::Aac, OfferedAacProfile::He)
    } else if codec.starts_with("mp4a.40.2") || matches!(family, "AAX" | "AAC" | "MP4A") {
        (OfferedCodec::Aac, OfferedAacProfile::Lc)
    } else if codec.starts_with("mp3") || codec.starts_with("mpeg") || family == "MPEG" {
        (OfferedCodec::Mp3, OfferedAacProfile::Unknown)
    } else {
        return None;
    };
    Some(QualityOffer {
        codec,
        aac_profile,
        bitrate_kbps,
        sample_rate_hz,
        channels: None,
    })
}

/// The sample rate a content format's kHz field names (`44` is 44.1 kHz).
fn sample_rate_of(khz: &str) -> Option<u32> {
    match khz {
        "8" => Some(8_000),
        "11" => Some(11_025),
        "16" => Some(16_000),
        "22" => Some(22_050),
        "24" => Some(24_000),
        "32" => Some(32_000),
        "44" => Some(44_100),
        "48" => Some(48_000),
        _ => None,
    }
}

/// Answers `qualityOffers` for every title in `params`, one metadata
/// request each, in order.
///
/// A title whose request fails or whose answer names no usable stream is
/// answered "not known"; a throttled request stops the batch, since asking
/// again would only be refused.
///
/// # Arguments
///
/// * `client` - A signed-in client for the account.
/// * `marketplace` - The account's marketplace country code.
/// * `params` - The titles and the instance's settings.
/// * `options` - The download options the instance's settings give.
///
/// # Errors
///
/// Returns [`AudibleError::Throttled`] when Audible throttles the account.
pub async fn quality_offers_with_client(
    client: &Client,
    marketplace: &str,
    params: &QualityOffersParams,
    options: &DownloadOptions,
) -> Result<QualityOffersResult> {
    let drm_type = if options.widevine { "Widevine" } else { "Adrm" };
    let quality = Quality::from(options.quality).api_value();
    let mut offers = Vec::with_capacity(params.title_ids.len());
    for title_id in &params.title_ids {
        let answer = match downloader::request_content_metadata(
            client,
            marketplace,
            title_id,
            drm_type,
            quality,
            "Flat",
        )
        .await
        {
            Ok(metadata) => metadata
                .get("content_reference")
                .and_then(|reference| offer_from_reference(reference, options.xhe_aac)),
            Err(err) => {
                let message = err.to_string();
                if bookclerk_storefront::is_throttle_reason(&message) {
                    return Err(AudibleError::Throttled(message));
                }
                tracing::debug!(title_id, error = %message, "no quality offer for title");
                None
            }
        };
        offers.push(match answer {
            Some(offer) => TitleQualityOffer { found: true, offer },
            None => TitleQualityOffer::default(),
        });
    }
    Ok(QualityOffersResult {
        offers,
        refreshed_credentials: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_codec_rate_and_bitrate_from_the_reference() {
        let offer = offer_from_reference(
            &json!({"codec": "mp4a.40.2", "content_format": "AAX_44_128"}),
            false,
        )
        .expect("an AAC-LC offer");
        assert_eq!(offer.codec, OfferedCodec::Aac);
        assert_eq!(offer.aac_profile, OfferedAacProfile::Lc);
        assert_eq!(offer.bitrate_kbps, Some(128));
        assert_eq!(offer.sample_rate_hz, Some(44_100));

        let low = offer_from_reference(&json!({"content_format": "AAX_22_64"}), false)
            .expect("a format alone names AAC-LC");
        assert_eq!(low.bitrate_kbps, Some(64));
        assert_eq!(low.sample_rate_hz, Some(22_050));

        let mp3 = offer_from_reference(&json!({"content_format": "MPEG"}), false).expect("MP3");
        assert_eq!(mp3.codec, OfferedCodec::Mp3);
        assert_eq!(mp3.bitrate_kbps, None);

        let he = offer_from_reference(&json!({"codec": "mp4a.40.5"}), false).expect("HE");
        assert_eq!(he.aac_profile, OfferedAacProfile::He);
        let he2 = offer_from_reference(&json!({"codec": "mp4a.40.29"}), false).expect("HEv2");
        assert_eq!(he2.aac_profile, OfferedAacProfile::HeV2);
    }

    #[test]
    fn xhe_is_offered_only_when_the_download_asks_for_it() {
        let reference = json!({"codec": "mp4a.40.42", "content_format": "XHE_44_137"});
        assert_eq!(offer_from_reference(&reference, false), None);
        let offer = offer_from_reference(&reference, true).expect("xHE-AAC");
        assert_eq!(offer.aac_profile, OfferedAacProfile::Xhe);
        assert_eq!(offer.bitrate_kbps, Some(137));
    }

    #[test]
    fn spatial_and_unknown_streams_are_never_offered() {
        for reference in [
            json!({"codec": "ec+3", "content_format": "EC3_48_768"}),
            json!({"codec": "ac-4", "content_format": "AC4_48_326"}),
            json!({"content_format": "ATMOS_48_768"}),
            json!({"codec": "flac9"}),
            json!({}),
        ] {
            assert_eq!(offer_from_reference(&reference, true), None, "{reference}");
        }
        let odd = offer_from_reference(&json!({"content_format": "AAX_99_99999"}), false)
            .expect("an AAC-LC offer with unreadable numbers");
        assert_eq!(odd.sample_rate_hz, None);
        assert_eq!(odd.bitrate_kbps, None);
    }
}
