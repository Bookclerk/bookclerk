//! Validation of one `ContentSource.qualityOffers` reply before the host
//! compares it with the files it holds.
//!
//! The reply is bytes a jailed storefront guest produced, so the host checks
//! it here, sans-IO, against the titles it asked about and the stream bounds
//! below, before any offer can queue an upgrade. The `quality_offers` fuzz
//! target drives this function (`fuzz/oracles/quality_offers.rs`) with the
//! same code the host runs.

use crate::scan_page::{validate_refreshed_credentials, ScanPageError};
use crate::{OfferedAacProfile, OfferedCodec, PluginError, QualityOffer, QualityOffersResult};

/// Most channels an offered stream may claim, the same bound the host puts
/// on a probed file.
pub const MAX_OFFER_CHANNELS: u32 = 32;

/// Highest sample rate in Hz an offered stream may claim, the same bound the
/// host puts on a probed file.
pub const MAX_OFFER_SAMPLE_RATE_HZ: u32 = 768_000;

/// Highest average bitrate in kbit/s an offered stream may claim: 32
/// channels of 32-bit PCM at 768 kHz, rounded up, so any real stream fits.
pub const MAX_OFFER_BITRATE_KBPS: u32 = 800_000;

/// The field of an offer that broke a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferField {
    /// `channels` was above [`MAX_OFFER_CHANNELS`].
    Channels,
    /// `sampleRateHz` was above [`MAX_OFFER_SAMPLE_RATE_HZ`].
    SampleRate,
    /// `bitrateKbps` was above [`MAX_OFFER_BITRATE_KBPS`].
    Bitrate,
    /// `aacProfile` was set on a codec that is not AAC.
    AacProfile,
}

impl OfferField {
    /// The field's wire name, for messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Channels => "channels",
            Self::SampleRate => "sampleRateHz",
            Self::Bitrate => "bitrateKbps",
            Self::AacProfile => "aacProfile",
        }
    }
}

/// Why the host refused a `qualityOffers` reply; no offer from a refused
/// reply is compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualityOffersError {
    /// The reply answered more titles than the host asked about.
    TooManyOffers {
        /// Answers the reply carried.
        count: usize,
        /// Titles the host sent.
        sent: usize,
    },
    /// A found offer broke a stream bound.
    Offer {
        /// Index of the offending answer in `offers`.
        index: usize,
        /// The field out of bounds.
        field: OfferField,
    },
    /// `refreshedCredentials` was too large or not one JSON object.
    RefreshedCredentials(ScanPageError),
}

impl std::fmt::Display for QualityOffersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyOffers { count, sent } => write!(
                f,
                "quality offers answer {count} titles, but {sent} were asked about"
            ),
            Self::Offer { index, field } => write!(
                f,
                "quality offer {index} has an impossible {}",
                field.as_str()
            ),
            Self::RefreshedCredentials(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for QualityOffersError {}

impl From<QualityOffersError> for PluginError {
    fn from(err: QualityOffersError) -> Self {
        Self::invalid_params(err.to_string())
    }
}

/// The first bound `offer` breaks, if any.
fn offer_fault(offer: &QualityOffer) -> Option<OfferField> {
    if offer.channels.is_some_and(|n| n > MAX_OFFER_CHANNELS) {
        return Some(OfferField::Channels);
    }
    if offer
        .sample_rate_hz
        .is_some_and(|hz| hz > MAX_OFFER_SAMPLE_RATE_HZ)
    {
        return Some(OfferField::SampleRate);
    }
    if offer
        .bitrate_kbps
        .is_some_and(|kbps| kbps > MAX_OFFER_BITRATE_KBPS)
    {
        return Some(OfferField::Bitrate);
    }
    if offer.codec != OfferedCodec::Aac && offer.aac_profile != OfferedAacProfile::Unknown {
        return Some(OfferField::AacProfile);
    }
    None
}

/// Checks one `qualityOffers` reply against the titles it answers and the
/// stream bounds.
///
/// Only answers marked `found` are checked; the host ignores the offer of
/// one that is not.
///
/// # Arguments
///
/// * `result` - The decoded reply (untrusted).
/// * `sent` - How many title ids the host sent.
///
/// # Errors
///
/// Returns [`QualityOffersError::TooManyOffers`] when the reply answers more
/// titles than were sent, [`QualityOffersError::Offer`] for the first found
/// offer out of bounds, and [`QualityOffersError::RefreshedCredentials`] when
/// the refreshed credentials are too large or not a JSON object.
pub fn validate_quality_offers(
    result: &QualityOffersResult,
    sent: usize,
) -> Result<(), QualityOffersError> {
    if result.offers.len() > sent {
        return Err(QualityOffersError::TooManyOffers {
            count: result.offers.len(),
            sent,
        });
    }
    for (index, answer) in result.offers.iter().enumerate() {
        if !answer.found {
            continue;
        }
        if let Some(field) = offer_fault(&answer.offer) {
            return Err(QualityOffersError::Offer { index, field });
        }
    }
    if let Some(blob) = result.refreshed_credentials.as_deref() {
        validate_refreshed_credentials(blob).map_err(QualityOffersError::RefreshedCredentials)?;
    }
    Ok(())
}

/// Decodes an unpacked single-segment Cap'n Proto message whose root is a
/// `QualityOffersResult`, with a traversal budget derived from the input
/// size.
///
/// The host decodes replies through the RPC layer; this byte entry point
/// serves the `quality_offers` fuzz target and its corpus replay.
///
/// # Arguments
///
/// * `bytes` - The message bytes (untrusted).
///
/// # Returns
///
/// The decoded reply.
///
/// # Errors
///
/// Returns the Cap'n Proto error the framing or the codec reports.
pub fn decode_quality_offers_bytes(bytes: &[u8]) -> capnp::Result<QualityOffersResult> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    crate::read_quality_offers_result(
        reader.get_root::<crate::plugin_capnp::quality_offers_result::Reader<'_>>()?,
    )
}

/// Encodes `result` as an unpacked single-segment Cap'n Proto message, the
/// inverse of [`decode_quality_offers_bytes`].
///
/// # Arguments
///
/// * `result` - The reply to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_quality_offers_bytes(result: &QualityOffersResult) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_quality_offers_result(
        message.init_root::<crate::plugin_capnp::quality_offers_result::Builder<'_>>(),
        result,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::TitleQualityOffer;

    fn found(offer: QualityOffer) -> TitleQualityOffer {
        TitleQualityOffer { found: true, offer }
    }

    fn aac(kbps: u32) -> QualityOffer {
        QualityOffer {
            codec: OfferedCodec::Aac,
            aac_profile: OfferedAacProfile::Lc,
            bitrate_kbps: Some(kbps),
            sample_rate_hz: Some(44_100),
            channels: Some(2),
        }
    }

    #[test]
    fn accepts_offers_at_every_bound() {
        let result = QualityOffersResult {
            offers: vec![
                found(QualityOffer {
                    codec: OfferedCodec::Flac,
                    aac_profile: OfferedAacProfile::Unknown,
                    bitrate_kbps: Some(MAX_OFFER_BITRATE_KBPS),
                    sample_rate_hz: Some(MAX_OFFER_SAMPLE_RATE_HZ),
                    channels: Some(MAX_OFFER_CHANNELS),
                }),
                TitleQualityOffer::default(),
            ],
            refreshed_credentials: Some(br#"{"token":"t"}"#.to_vec()),
        };
        assert_eq!(validate_quality_offers(&result, 2), Ok(()));
        assert_eq!(validate_quality_offers(&result, 3), Ok(()));
    }

    #[test]
    fn refuses_more_answers_than_titles() {
        let result = QualityOffersResult {
            offers: vec![found(aac(128)), found(aac(64))],
            refreshed_credentials: None,
        };
        assert_eq!(
            validate_quality_offers(&result, 1),
            Err(QualityOffersError::TooManyOffers { count: 2, sent: 1 })
        );
    }

    #[test]
    fn refuses_an_impossible_found_offer_and_ignores_unfound_ones() {
        let cases = [
            (
                QualityOffer {
                    channels: Some(MAX_OFFER_CHANNELS + 1),
                    ..aac(128)
                },
                OfferField::Channels,
            ),
            (
                QualityOffer {
                    sample_rate_hz: Some(MAX_OFFER_SAMPLE_RATE_HZ + 1),
                    ..aac(128)
                },
                OfferField::SampleRate,
            ),
            (aac(MAX_OFFER_BITRATE_KBPS + 1), OfferField::Bitrate),
            (
                QualityOffer {
                    codec: OfferedCodec::Mp3,
                    ..aac(128)
                },
                OfferField::AacProfile,
            ),
        ];
        for (offer, field) in cases {
            let refused = QualityOffersResult {
                offers: vec![found(aac(64)), found(offer.clone())],
                refreshed_credentials: None,
            };
            assert_eq!(
                validate_quality_offers(&refused, 2),
                Err(QualityOffersError::Offer { index: 1, field })
            );
            let unfound = QualityOffersResult {
                offers: vec![TitleQualityOffer {
                    found: false,
                    offer,
                }],
                refreshed_credentials: None,
            };
            assert_eq!(validate_quality_offers(&unfound, 1), Ok(()));
        }
    }

    #[test]
    fn refuses_refreshed_credentials_that_are_not_an_object() {
        let result = QualityOffersResult {
            offers: Vec::new(),
            refreshed_credentials: Some(b"[1]".to_vec()),
        };
        assert_eq!(
            validate_quality_offers(&result, 0),
            Err(QualityOffersError::RefreshedCredentials(
                ScanPageError::RefreshedCredentialsNotObject
            ))
        );
        assert!(
            PluginError::from(validate_quality_offers(&result, 0).unwrap_err())
                .to_string()
                .contains("JSON object")
        );
    }

    #[test]
    fn bytes_round_trip() {
        let result = QualityOffersResult {
            offers: vec![found(aac(128)), TitleQualityOffer::default()],
            refreshed_credentials: Some(br#"{"a":1}"#.to_vec()),
        };
        let bytes = encode_quality_offers_bytes(&result).expect("encode");
        assert_eq!(decode_quality_offers_bytes(&bytes).expect("decode"), result);
        assert!(decode_quality_offers_bytes(&[1, 2, 3]).is_err());
    }
}
