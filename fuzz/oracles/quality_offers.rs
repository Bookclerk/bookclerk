//! Shared oracle for the `quality_offers` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one `ContentSource.qualityOffers` reply a storefront guest
//! could return, framed with the request it answers: byte 0 is how many
//! title ids the host sent, and the rest is the unpacked
//! `QualityOffersResult` message. The invariants:
//!
//! - decoding and validation never panic;
//! - [`validate_quality_offers`] accepts the reply exactly when the
//!   independent recount below finds every rule kept: no more answers than
//!   titles sent; every found offer within the channel, sample-rate and
//!   bitrate caps, with an AAC profile only on AAC; and any
//!   `refreshedCredentials` at most `maxRefreshedCredentialBytes` and one
//!   JSON object (read here as a typed map rather than an untyped value);
//! - an accepted reply re-encodes, decodes back equal, and is accepted again.

use bookclerk_plugin_abi::{
    decode_quality_offers_bytes, encode_quality_offers_bytes, validate_quality_offers,
    OfferedAacProfile, OfferedCodec, QualityOffersResult, MAX_OFFER_BITRATE_KBPS,
    MAX_OFFER_CHANNELS, MAX_OFFER_SAMPLE_RATE_HZ, MAX_REFRESHED_CREDENTIAL_BYTES,
};

/// Largest input worth decoding; real replies at every cap are far smaller.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// The reply rules restated directly: true when the reply keeps them all.
fn keeps_every_rule(result: &QualityOffersResult, sent: usize) -> bool {
    result.offers.len() <= sent
        && result.offers.iter().filter(|a| a.found).all(|a| {
            let o = &a.offer;
            o.channels.unwrap_or(0) <= MAX_OFFER_CHANNELS
                && o.sample_rate_hz.unwrap_or(0) <= MAX_OFFER_SAMPLE_RATE_HZ
                && o.bitrate_kbps.unwrap_or(0) <= MAX_OFFER_BITRATE_KBPS
                && (o.codec == OfferedCodec::Aac || o.aac_profile == OfferedAacProfile::Unknown)
        })
        && result.refreshed_credentials.as_deref().is_none_or(|blob| {
            blob.is_empty()
                || (u32::try_from(blob.len()).is_ok_and(|n| n <= MAX_REFRESHED_CREDENTIAL_BYTES)
                    && serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(blob)
                        .is_ok())
        })
}

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&sent, message)) = data.split_first() else {
        return;
    };
    let sent = usize::from(sent);
    let Ok(result) = decode_quality_offers_bytes(message) else {
        return;
    };
    let verdict = validate_quality_offers(&result, sent);
    assert_eq!(
        verdict.is_ok(),
        keeps_every_rule(&result, sent),
        "validation disagrees with the oracle: {verdict:?}"
    );
    if verdict.is_ok() {
        let bytes = encode_quality_offers_bytes(&result)
            .unwrap_or_else(|err| panic!("an accepted reply must encode: {err}"));
        let again = decode_quality_offers_bytes(&bytes)
            .unwrap_or_else(|err| panic!("an encoded reply must decode: {err}"));
        assert_eq!(again, result, "an accepted reply round-trips");
        assert!(validate_quality_offers(&again, sent).is_ok());
    }
}
