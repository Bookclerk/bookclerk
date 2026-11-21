//! When a store's offer is a better file than the one the library holds.
//!
//! The quality-upgrade check asks a store which stream an account would
//! download for a title today (`ContentSource.qualityOffers`) and compares
//! it with the original copy the library keeps, from the stream details the
//! acquire probe stored (`edition_files`). [`is_upgrade`] is the rule; the
//! daemon's upgrade scan decides what to ask and queues the downloads.

use bookclerk_config::CopyQuality;
use bookclerk_tags::{AacProfile, AudioCodec};

use crate::store::{FileRole, ProbedAudioFile};

/// The bitrate step that makes the same class of codec worth downloading
/// again: an offer must be at least this many hundredths of the held
/// bitrate (115 = 1.15×).
pub const SAME_CLASS_STEP_PERCENT: u64 = 115;

/// A stream as the comparison sees it: the codec, the AAC profile when the
/// codec is AAC, and the average bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamQuality {
    /// The audio encoding.
    pub codec: AudioCodec,
    /// The AAC profile; `None` for other codecs or when not known.
    pub aac_profile: Option<AacProfile>,
    /// Average bitrate in kbit/s; `None` when not known.
    pub bitrate_kbps: Option<u32>,
}

/// How good a codec is at a given bitrate, coarsely: a higher class sounds
/// better at the same bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CodecClass {
    /// MPEG audio (MP3 and its older layers).
    Mpeg,
    /// AAC-LC, HE-AAC and Vorbis.
    Transform,
    /// xHE-AAC and Opus.
    Modern,
    /// FLAC and Apple Lossless.
    Lossless,
}

/// The class of `stream`'s codec; `None` for codecs the check never trades
/// to or from (the Dolby codecs carry spatial mixes the library does not
/// fetch).
fn class(stream: &StreamQuality) -> Option<CodecClass> {
    match stream.codec {
        AudioCodec::Mp3 => Some(CodecClass::Mpeg),
        AudioCodec::Aac => match stream.aac_profile {
            Some(AacProfile::Xhe) => Some(CodecClass::Modern),
            Some(AacProfile::Lc | AacProfile::He | AacProfile::HeV2) | None => {
                Some(CodecClass::Transform)
            }
        },
        AudioCodec::Vorbis => Some(CodecClass::Transform),
        AudioCodec::Opus => Some(CodecClass::Modern),
        AudioCodec::Flac | AudioCodec::Alac => Some(CodecClass::Lossless),
        AudioCodec::Eac3 | AudioCodec::Ac4 => None,
    }
}

/// True when downloading `offer` would give a better file than `held`.
///
/// - A lossless offer beats any lossy file, whatever the bitrates; nothing
///   beats a lossless file.
/// - A better class of codec (MP3, then AAC-LC / HE-AAC / Vorbis, then
///   xHE-AAC / Opus) wins at the same or a higher bitrate.
/// - The same class wins at [`SAME_CLASS_STEP_PERCENT`] of the held bitrate
///   or more.
/// - Anything else, including an offer or a held file whose bitrate is not
///   known, or a Dolby codec on either side, is not an upgrade.
#[must_use]
pub fn is_upgrade(held: &StreamQuality, offer: &StreamQuality) -> bool {
    let (Some(held_class), Some(offer_class)) = (class(held), class(offer)) else {
        return false;
    };
    if held_class == CodecClass::Lossless {
        return false;
    }
    if offer_class == CodecClass::Lossless {
        return true;
    }
    let (Some(held_kbps), Some(offer_kbps)) = (held.bitrate_kbps, offer.bitrate_kbps) else {
        return false;
    };
    let (held_kbps, offer_kbps) = (u64::from(held_kbps), u64::from(offer_kbps));
    match offer_class.cmp(&held_class) {
        std::cmp::Ordering::Greater => offer_kbps >= held_kbps,
        std::cmp::Ordering::Equal => {
            held_kbps > 0 && offer_kbps * 100 >= held_kbps * SAME_CLASS_STEP_PERCENT
        }
        std::cmp::Ordering::Less => false,
    }
}

/// The stream the library holds for an edition, from its audio files: the
/// original-quality copy's files (what the store delivered, before any
/// re-encode), all in one codec, at the lowest bitrate among them.
///
/// # Returns
///
/// `None` when no original-quality file was probed with a codec, or when
/// its files disagree on codec or AAC profile.
#[must_use]
pub fn held_stream(files: &[ProbedAudioFile]) -> Option<StreamQuality> {
    let mut held: Option<StreamQuality> = None;
    let originals = files.iter().filter(|file| {
        matches!(
            file.role,
            FileRole::Audio | FileRole::Part | FileRole::Chapter
        ) && file
            .copy
            .as_ref()
            .is_some_and(|copy| copy.quality == CopyQuality::ORIGINAL)
    });
    for file in originals {
        let codec = file.stream.codec?;
        let bitrate = file.probe.as_ref().and_then(|probe| probe.bitrate_kbps);
        let this = StreamQuality {
            codec,
            aac_profile: file.stream.format.aac_profile,
            bitrate_kbps: bitrate,
        };
        held = match held {
            None => Some(this),
            Some(seen) if seen.codec == this.codec && seen.aac_profile == this.aac_profile => {
                Some(StreamQuality {
                    bitrate_kbps: match (seen.bitrate_kbps, this.bitrate_kbps) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        _ => None,
                    },
                    ..seen
                })
            }
            Some(_) => return None,
        };
    }
    held
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(codec: AudioCodec, profile: Option<AacProfile>, kbps: Option<u32>) -> StreamQuality {
        StreamQuality {
            codec,
            aac_profile: profile,
            bitrate_kbps: kbps,
        }
    }

    fn lc(kbps: u32) -> StreamQuality {
        stream(AudioCodec::Aac, Some(AacProfile::Lc), Some(kbps))
    }

    #[test]
    fn the_same_class_needs_fifteen_percent_more() {
        assert!(is_upgrade(&lc(64), &lc(128)));
        assert!(is_upgrade(&lc(100), &lc(115)));
        assert!(!is_upgrade(&lc(100), &lc(114)));
        assert!(!is_upgrade(&lc(128), &lc(128)));
        assert!(!is_upgrade(&lc(128), &lc(64)));
        // HE-AAC and Vorbis sit in AAC-LC's class.
        let vorbis = stream(AudioCodec::Vorbis, None, Some(160));
        assert!(is_upgrade(&lc(128), &vorbis));
        let he = stream(AudioCodec::Aac, Some(AacProfile::He), Some(64));
        assert!(!is_upgrade(&he, &lc(64)));
    }

    #[test]
    fn a_better_codec_wins_at_the_same_bitrate_or_more() {
        let mp3 = stream(AudioCodec::Mp3, None, Some(128));
        assert!(is_upgrade(&mp3, &lc(128)));
        assert!(!is_upgrade(&mp3, &lc(127)));
        let xhe = stream(AudioCodec::Aac, Some(AacProfile::Xhe), Some(137));
        assert!(is_upgrade(&lc(128), &xhe));
        assert!(!is_upgrade(&xhe, &lc(256)), "a worse codec never wins");
        let opus = stream(AudioCodec::Opus, None, Some(128));
        assert!(is_upgrade(&lc(128), &opus));
    }

    #[test]
    fn lossless_beats_lossy_and_nothing_beats_lossless() {
        let flac = stream(AudioCodec::Flac, None, None);
        assert!(is_upgrade(&lc(320), &flac));
        assert!(is_upgrade(&stream(AudioCodec::Mp3, None, None), &flac));
        let alac = stream(AudioCodec::Alac, None, Some(900));
        assert!(!is_upgrade(&flac, &alac));
        assert!(!is_upgrade(
            &alac,
            &stream(AudioCodec::Flac, None, Some(2_000))
        ));
    }

    fn file(
        role: FileRole,
        quality: CopyQuality,
        codec: Option<AudioCodec>,
        profile: Option<AacProfile>,
        kbps: Option<u32>,
    ) -> ProbedAudioFile {
        ProbedAudioFile {
            destination: "local".into(),
            storage_key: "k".into(),
            role,
            copy: Some(bookclerk_config::OutputCopy {
                format: bookclerk_config::CopyFormat::M4b,
                quality,
            }),
            probe: Some(crate::FileProbe {
                content_type: "audio/mp4".into(),
                bitrate_kbps: kbps,
                lossless: false,
                source_store: None,
            }),
            stream: crate::AudioStream {
                codec,
                format: bookclerk_tags::AudioFormat {
                    aac_profile: profile,
                    ..bookclerk_tags::AudioFormat::default()
                },
                duration_ms: None,
            },
        }
    }

    #[test]
    fn the_held_stream_is_the_original_copy_at_its_lowest_bitrate() {
        let aac = Some(AudioCodec::Aac);
        let lc_profile = Some(AacProfile::Lc);
        let low = CopyQuality::parse("64k").expect("a re-encode quality");
        let files = vec![
            file(
                FileRole::Part,
                CopyQuality::ORIGINAL,
                aac,
                lc_profile,
                Some(128),
            ),
            file(
                FileRole::Part,
                CopyQuality::ORIGINAL,
                aac,
                lc_profile,
                Some(125),
            ),
            // A re-encoded copy and a cover are not what the store delivered.
            file(FileRole::Audio, low, Some(AudioCodec::Mp3), None, Some(64)),
            file(FileRole::Cover, CopyQuality::ORIGINAL, None, None, None),
        ];
        assert_eq!(held_stream(&files), Some(lc(125)));

        let mixed = vec![
            file(
                FileRole::Part,
                CopyQuality::ORIGINAL,
                aac,
                lc_profile,
                Some(128),
            ),
            file(
                FileRole::Part,
                CopyQuality::ORIGINAL,
                Some(AudioCodec::Mp3),
                None,
                Some(128),
            ),
        ];
        assert_eq!(held_stream(&mixed), None, "parts disagree on codec");
        let unprobed = vec![file(
            FileRole::Audio,
            CopyQuality::ORIGINAL,
            None,
            None,
            Some(64),
        )];
        assert_eq!(held_stream(&unprobed), None);
        let one_unknown = vec![
            file(
                FileRole::Part,
                CopyQuality::ORIGINAL,
                aac,
                lc_profile,
                Some(128),
            ),
            file(FileRole::Part, CopyQuality::ORIGINAL, aac, lc_profile, None),
        ];
        assert_eq!(
            held_stream(&one_unknown),
            Some(stream(AudioCodec::Aac, lc_profile, None))
        );
        assert_eq!(held_stream(&[]), None);
    }

    #[test]
    fn unknown_bitrates_and_dolby_codecs_never_upgrade() {
        assert!(!is_upgrade(
            &stream(AudioCodec::Aac, Some(AacProfile::Lc), None),
            &lc(256)
        ));
        assert!(!is_upgrade(
            &lc(64),
            &stream(AudioCodec::Aac, Some(AacProfile::Lc), None)
        ));
        let eac3 = stream(AudioCodec::Eac3, None, Some(768));
        assert!(!is_upgrade(&lc(64), &eac3));
        assert!(!is_upgrade(&eac3, &lc(768)));
        assert!(!is_upgrade(
            &lc(64),
            &stream(AudioCodec::Ac4, None, Some(320))
        ));
    }
}
