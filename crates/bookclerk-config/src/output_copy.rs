//! The copies of each book a library keeps (`output.copies`).
//!
//! A library packages every acquired book as its main output
//! ([`crate::OutputConfig::format`] with the `lame` settings), and may keep
//! further copies of it in other formats or at other qualities: an MP3 at
//! 64 kbit/s mono for an old player beside the M4B, say. Each copy is named
//! by a [`CopyFormat`] and a [`CopyQuality`]; together they are its
//! [`OutputCopy`]. Destination filters choose copies by those two names
//! (the `format` and `quality` rule fields in [`crate::placement`]), and
//! every file row of a copy carries them.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::extras::LameConfig;
use crate::pipeline_opts::OutputFormat;

/// Most copies a library may keep beside its main output.
pub const MAX_EXTRA_COPIES: usize = 8;

/// Longest quality name, in bytes (`320k-mono` is nine).
pub const MAX_QUALITY_BYTES: usize = 16;

/// The quality of the main output when it is Opus (`format = "opus"`):
/// 64 kbit/s stereo. Another bitrate takes an Opus extra copy beside an
/// M4B main output.
pub const OPUS_MAIN_KBPS: u16 = 64;

/// The bitrates, in kbit/s, an Opus copy is named by. Opus encodes in
/// variable bitrate around the target, so these are targets, not caps.
pub const OPUS_KBPS: [u16; 7] = [16, 24, 32, 48, 64, 96, 128];

/// The Opus encoder settings one output is written with.
///
/// The main output uses [`OPUS_MAIN_KBPS`] stereo; an Opus copy takes the
/// bitrate and channel choice its quality names ([`OutputCopy::opus_kbps`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpusSettings {
    /// Target bitrate in kbit/s; Opus encodes in variable bitrate around it.
    pub kbps: u16,
    /// When true, the encoder downmixes to one channel.
    pub mono: bool,
}

impl Default for OpusSettings {
    fn default() -> Self {
        Self {
            kbps: OPUS_MAIN_KBPS,
            mono: false,
        }
    }
}

/// The constant bitrates, in kbit/s, the MP3 encoder writes.
///
/// `bookclerk-media` rounds any other requested bitrate up to the next of
/// these (and anything above 256 to 320), so a quality name only offers
/// these rates: the name then always describes the audio written.
pub const MP3_CBR_KBPS: [u16; 7] = [64, 96, 128, 160, 192, 256, 320];

/// The constant bitrate, in kbit/s, the MP3 encoder writes for a requested
/// `kbps`: the smallest of [`MP3_CBR_KBPS`] at or above it, else 320.
#[must_use]
pub fn mp3_cbr_kbps(kbps: u32) -> u16 {
    MP3_CBR_KBPS
        .into_iter()
        .find(|rate| u32::from(*rate) >= kbps)
        .unwrap_or(320)
}

/// The format of one copy of a book.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CopyFormat {
    /// One M4B with tags and chapters (output format `enriched_m4b`).
    M4b,
    /// One MP3 (output format `single_mp3`).
    Mp3,
    /// One MP3 per chapter (output format `split_mp3_by_chapter`).
    Mp3Chapters,
    /// The audio as the store delivered it (output format `none`).
    Original,
    /// One Ogg Opus file with tags and chapters (output format `opus`), an
    /// advanced format.
    Opus,
}

impl CopyFormat {
    /// Every format, in the order the names are documented.
    pub const ALL: [Self; 5] = [
        Self::M4b,
        Self::Mp3,
        Self::Mp3Chapters,
        Self::Original,
        Self::Opus,
    ];

    /// The format's name in settings, filter rules and file rows.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::M4b => "m4b",
            Self::Mp3 => "mp3",
            Self::Mp3Chapters => "mp3_chapters",
            Self::Original => "original",
            Self::Opus => "opus",
        }
    }

    /// True for the advanced formats: ones players and servers read less
    /// widely than M4B and MP3, which settings screens list apart from the
    /// common ones.
    #[must_use]
    pub fn is_advanced(self) -> bool {
        match self {
            Self::Opus => true,
            Self::M4b | Self::Mp3 | Self::Mp3Chapters | Self::Original => false,
        }
    }

    /// Reads a format name, without case; `None` for any other text.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        Self::ALL
            .into_iter()
            .find(|format| format.as_str().eq_ignore_ascii_case(raw))
    }

    /// The copy format an output format produces; `None` for the format
    /// the acquire pipeline refuses (`split_mp3_by_size`).
    #[must_use]
    pub fn of_output(format: OutputFormat) -> Option<Self> {
        match format {
            OutputFormat::EnrichedM4b => Some(Self::M4b),
            OutputFormat::SingleMp3 => Some(Self::Mp3),
            OutputFormat::SplitMp3ByChapter => Some(Self::Mp3Chapters),
            OutputFormat::None => Some(Self::Original),
            OutputFormat::Opus => Some(Self::Opus),
            OutputFormat::SplitMp3BySize => None,
        }
    }

    /// The output format the acquire pipeline packages this copy with.
    #[must_use]
    pub fn output(self) -> OutputFormat {
        match self {
            Self::M4b => OutputFormat::EnrichedM4b,
            Self::Mp3 => OutputFormat::SingleMp3,
            Self::Mp3Chapters => OutputFormat::SplitMp3ByChapter,
            Self::Original => OutputFormat::None,
            Self::Opus => OutputFormat::Opus,
        }
    }

    /// True for the formats an encoder writes (MP3, Opus), whose quality
    /// is an encoder setting rather than `original`.
    #[must_use]
    pub fn is_encoded(self) -> bool {
        match self {
            Self::Mp3 | Self::Mp3Chapters | Self::Opus => true,
            Self::M4b | Self::Original => false,
        }
    }
}

impl fmt::Display for CopyFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an encoded copy's audio is rated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Rate {
    /// No re-encode beyond what the format needs: the store's audio.
    Original,
    /// A constant bitrate in kbit/s, one of [`MP3_CBR_KBPS`].
    Kbps(u16),
    /// A LAME VBR quality, 0 (best) to 9.
    Vbr(u8),
}

/// The quality of one copy, by name.
///
/// The names are `original` (the store's audio, re-packaged without
/// re-encoding where the format allows), a bitrate such as `64k` (one of
/// the MP3 encoder's constant rates, [`MP3_CBR_KBPS`], or an Opus target,
/// [`OPUS_KBPS`]) or a LAME VBR quality `v0` to `v9`; either of the last
/// two may end in `-mono` for a copy mixed down to one channel. Which names
/// suit which format is [`OutputCopy::validate`]'s to say. Names are stored
/// in lower case.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CopyQuality {
    /// The rate the name gives.
    rate: Rate,
    /// True for a copy mixed down to one channel (`-mono`).
    mono: bool,
}

impl CopyQuality {
    /// The `original` quality.
    pub const ORIGINAL: Self = Self {
        rate: Rate::Original,
        mono: false,
    };

    /// Reads a quality name, without case; `None` for any text that is not
    /// one (see the type's documentation).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().to_ascii_lowercase();
        if raw.len() > MAX_QUALITY_BYTES {
            return None;
        }
        if raw == "original" {
            return Some(Self::ORIGINAL);
        }
        let (rate, mono) = match raw.strip_suffix("-mono") {
            Some(rate) => (rate, true),
            None => (raw.as_str(), false),
        };
        let rate = if let Some(kbps) = rate.strip_suffix('k') {
            let canonical = !kbps.is_empty()
                && kbps.bytes().all(|b| b.is_ascii_digit())
                && !kbps.starts_with('0');
            let kbps = kbps.parse::<u16>().ok().filter(|_| canonical)?;
            if !MP3_CBR_KBPS.contains(&kbps) && !OPUS_KBPS.contains(&kbps) {
                return None;
            }
            Rate::Kbps(kbps)
        } else {
            match rate.strip_prefix('v')?.as_bytes() {
                [digit @ b'0'..=b'9'] => Rate::Vbr(digit - b'0'),
                _ => return None,
            }
        };
        Some(Self { rate, mono })
    }

    /// True for `original`.
    #[must_use]
    pub fn is_original(&self) -> bool {
        self.rate == Rate::Original
    }

    /// True for a copy mixed down to one channel (`-mono`).
    #[must_use]
    pub fn is_mono(&self) -> bool {
        self.mono
    }

    /// The bitrate the name gives, in kbit/s; `None` for `original` and
    /// the VBR levels.
    #[must_use]
    pub fn kbps(&self) -> Option<u16> {
        match self.rate {
            Rate::Kbps(kbps) => Some(kbps),
            Rate::Original | Rate::Vbr(_) => None,
        }
    }

    /// The quality the library's LAME settings give an MP3.
    #[must_use]
    pub fn of_lame(lame: &LameConfig) -> Self {
        let mono = lame.downsample_mono || lame.mode.eq_ignore_ascii_case("mono");
        let rate = if lame.constant_bitrate || lame.target.eq_ignore_ascii_case("bitrate") {
            Rate::Kbps(mp3_cbr_kbps(lame.bitrate_kbps))
        } else {
            Rate::Vbr(lame.vbr_quality.min(9))
        };
        Self { rate, mono }
    }

    /// `base` with this quality's encoder settings: a bitrate selects a
    /// constant bitrate, a VBR level a VBR quality, and `-mono` mixes down.
    /// `original` leaves `base` as it is.
    #[must_use]
    pub fn lame(&self, base: &LameConfig) -> LameConfig {
        let mut lame = base.clone();
        match self.rate {
            Rate::Original => return lame,
            Rate::Kbps(kbps) => {
                lame.target = String::from("bitrate");
                lame.constant_bitrate = true;
                lame.bitrate_kbps = u32::from(kbps);
            }
            Rate::Vbr(level) => {
                lame.target = String::from("quality");
                lame.constant_bitrate = false;
                lame.vbr_quality = level;
            }
        }
        lame.downsample_mono = self.mono;
        lame.mode = String::from(if self.mono { "mono" } else { "default" });
        lame
    }
}

impl fmt::Display for CopyQuality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.rate {
            Rate::Original => return f.write_str("original"),
            Rate::Kbps(kbps) => write!(f, "{kbps}k")?,
            Rate::Vbr(level) => write!(f, "v{level}")?,
        }
        if self.mono {
            f.write_str("-mono")?;
        }
        Ok(())
    }
}

impl Serialize for CopyQuality {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CopyQuality {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{raw}` is not a copy quality (original, a bitrate such as 64k, \
                 or v0 to v9, with an optional -mono)"
            ))
        })
    }
}

impl utoipa::PartialSchema for CopyQuality {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .description(Some(
                "A copy's quality: `original`, a bitrate (for MP3 a constant rate \
                 the encoder writes: `64k`, `96k`, `128k`, `160k`, `192k`, `256k` or \
                 `320k`; for Opus a target: `16k`, `24k`, `32k`, `48k`, `64k`, `96k` \
                 or `128k`), or a LAME VBR quality `v0` to `v9` (MP3 only); the last \
                 two may end in `-mono`.",
            ))
            .pattern(Some(
                "^(original|((16|24|32|48|64|96|128|160|192|256|320)k|v[0-9])(-mono)?)$",
            ))
            .examples(["64k-mono"])
            .into()
    }
}

impl utoipa::ToSchema for CopyQuality {}

/// One copy of every acquired book: a format at a quality.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputCopy {
    /// The copy's format.
    pub format: CopyFormat,
    /// The copy's quality.
    pub quality: CopyQuality,
}

// Reads only from an object of exactly `format` and `quality`: an array of
// the two values, which serde's derive would also take, is refused, so the
// reader matches the documented shape.
impl<'de> Deserialize<'de> for OutputCopy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The two fields an output copy object holds.
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            /// `format`.
            Format,
            /// `quality`.
            Quality,
        }

        /// Reads an output copy from a map only.
        struct CopyVisitor;

        impl<'de> serde::de::Visitor<'de> for CopyVisitor {
            type Value = OutputCopy;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with `format` and `quality`")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<OutputCopy, A::Error> {
                let mut format = None;
                let mut quality = None;
                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::Format => {
                            if format.is_some() {
                                return Err(serde::de::Error::duplicate_field("format"));
                            }
                            format = Some(map.next_value()?);
                        }
                        Field::Quality => {
                            if quality.is_some() {
                                return Err(serde::de::Error::duplicate_field("quality"));
                            }
                            quality = Some(map.next_value()?);
                        }
                    }
                }
                Ok(OutputCopy {
                    format: format.ok_or_else(|| serde::de::Error::missing_field("format"))?,
                    quality: quality.ok_or_else(|| serde::de::Error::missing_field("quality"))?,
                })
            }
        }

        deserializer.deserialize_map(CopyVisitor)
    }
}

/// Why a copy list was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyError(String);

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CopyError {}

impl OutputCopy {
    /// The copy the library's main output makes: its format, with
    /// `original` quality for M4B and as-delivered output, the LAME
    /// settings' quality for MP3 and [`OPUS_MAIN_KBPS`] for Opus. `None`
    /// for an output format the acquire pipeline refuses.
    #[must_use]
    pub fn main(format: OutputFormat, lame: &LameConfig) -> Option<Self> {
        let format = CopyFormat::of_output(format)?;
        let quality = match format {
            CopyFormat::Mp3 | CopyFormat::Mp3Chapters => CopyQuality::of_lame(lame),
            CopyFormat::Opus => CopyQuality {
                rate: Rate::Kbps(OPUS_MAIN_KBPS),
                mono: false,
            },
            CopyFormat::M4b | CopyFormat::Original => CopyQuality::ORIGINAL,
        };
        Some(Self { format, quality })
    }

    /// Checks that the quality suits the format: M4B and as-delivered
    /// copies are `original` only (the M4B is re-packaged, not
    /// re-encoded), MP3 copies need one of the MP3 encoder's constant
    /// rates or a VBR quality, and Opus copies one of the Opus targets.
    ///
    /// # Errors
    ///
    /// Returns a [`CopyError`] naming the copy when they do not suit.
    pub fn validate(&self) -> Result<(), CopyError> {
        match (self.format.is_encoded(), self.quality.is_original()) {
            (true, true) => {
                return Err(CopyError(format!(
                    "a {} copy needs a bitrate{}, not `original`",
                    self.format,
                    if self.format == CopyFormat::Opus {
                        ""
                    } else {
                        " or a VBR quality"
                    }
                )))
            }
            (false, false) => {
                return Err(CopyError(format!(
                    "a {} copy keeps the store's audio: its quality is `original`, not `{}`",
                    self.format, self.quality
                )))
            }
            (true, false) | (false, true) => {}
        }
        let rates: &[u16] = match self.format {
            CopyFormat::Opus => &OPUS_KBPS,
            CopyFormat::Mp3 | CopyFormat::Mp3Chapters => &MP3_CBR_KBPS,
            CopyFormat::M4b | CopyFormat::Original => return Ok(()),
        };
        match (self.quality.rate, self.format) {
            (Rate::Kbps(kbps), _) if rates.contains(&kbps) => Ok(()),
            (Rate::Vbr(_), CopyFormat::Mp3 | CopyFormat::Mp3Chapters) | (Rate::Original, _) => {
                Ok(())
            }
            (Rate::Vbr(_), _) => Err(CopyError(format!(
                "a {} copy is named by a bitrate, not a VBR quality (`{}`)",
                self.format, self.quality
            ))),
            (Rate::Kbps(_), _) => Err(CopyError(format!(
                "`{}` is not a bitrate a {} copy is written at ({})",
                self.quality,
                self.format,
                rates
                    .iter()
                    .map(|kbps| format!("{kbps}k"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// The Opus encoder settings for an Opus copy at this quality; `None`
    /// for any other format or a quality without a bitrate.
    #[must_use]
    pub fn opus_kbps(&self) -> Option<(u16, bool)> {
        match self.format {
            CopyFormat::Opus => self.quality.kbps().map(|kbps| (kbps, self.quality.mono)),
            CopyFormat::M4b | CopyFormat::Mp3 | CopyFormat::Mp3Chapters | CopyFormat::Original => {
                None
            }
        }
    }

    /// The copy's label in a storage key, for a destination that keeps
    /// more than one copy of a book: `mp3 64k-mono`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} {}", self.format, self.quality)
    }

    /// Reads one copy written `format:quality` (`mp3:64k-mono`), as the
    /// `output.copies` setting lists them.
    ///
    /// # Errors
    ///
    /// Returns a [`CopyError`] for text that is not that shape, an unknown
    /// format or quality, or a quality that does not suit the format.
    pub fn parse(raw: &str) -> Result<Self, CopyError> {
        let raw = raw.trim();
        let (format, quality) = raw
            .split_once(':')
            .ok_or_else(|| CopyError(format!("a copy is written format:quality, not `{raw}`")))?;
        let format = CopyFormat::parse(format).ok_or_else(|| {
            CopyError(format!(
                "`{}` is not a copy format (m4b, mp3, mp3_chapters, original or opus)",
                format.trim()
            ))
        })?;
        let quality = CopyQuality::parse(quality)
            .ok_or_else(|| CopyError(format!("`{}` is not a copy quality", quality.trim())))?;
        let copy = Self { format, quality };
        copy.validate()?;
        Ok(copy)
    }
}

impl fmt::Display for OutputCopy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.format, self.quality)
    }
}

/// Checks a list of extra copies: at most [`MAX_EXTRA_COPIES`], each valid
/// ([`OutputCopy::validate`]), none given twice and none the main output's
/// copy `main`.
///
/// # Errors
///
/// Returns a [`CopyError`] for the first copy that breaks a rule.
pub fn validate_copies(main: Option<&OutputCopy>, copies: &[OutputCopy]) -> Result<(), CopyError> {
    if copies.len() > MAX_EXTRA_COPIES {
        return Err(CopyError(format!(
            "a library keeps at most {MAX_EXTRA_COPIES} copies beside its main output"
        )));
    }
    for (at, copy) in copies.iter().enumerate() {
        copy.validate()?;
        if main == Some(copy) {
            return Err(CopyError(format!(
                "the copy {copy} is the main output already"
            )));
        }
        if copies[..at].contains(copy) {
            return Err(CopyError(format!("the copy {copy} is listed twice")));
        }
    }
    Ok(())
}

/// Every copy an acquire makes, in order: the main output's copy first
/// (when the pipeline can make it), then each valid extra copy that
/// differs from every earlier one, at most [`MAX_EXTRA_COPIES`] of them
/// whatever the list holds.
#[must_use]
pub fn copies_to_make(main: Option<OutputCopy>, extra: &[OutputCopy]) -> Vec<OutputCopy> {
    let mut all: Vec<OutputCopy> = main.into_iter().collect();
    let mut extras = 0;
    for copy in extra {
        if extras == MAX_EXTRA_COPIES {
            break;
        }
        if copy.validate().is_ok() && !all.contains(copy) {
            all.push(copy.clone());
            extras += 1;
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(raw: &str) -> OutputCopy {
        OutputCopy::parse(raw).unwrap()
    }

    #[test]
    fn quality_names_round_trip_in_canonical_form() {
        for raw in ["original", "64k", "96k-mono", "320k", "v0", "v9-mono"] {
            let quality = CopyQuality::parse(raw).unwrap();
            assert_eq!(quality.to_string(), raw);
        }
        assert_eq!(
            CopyQuality::parse(" 64K-Mono ").unwrap().to_string(),
            "64k-mono"
        );
    }

    #[test]
    fn odd_quality_names_are_refused() {
        for raw in [
            "",
            "k",
            "064k",
            "7k",
            "8k",
            "40k",
            "80k",
            "321k",
            "1000k",
            "+64k",
            "64",
            "v",
            "v10",
            "va",
            "mono",
            "original-mono",
            "64k-mono-mono",
            "64kmono",
            "64k-stereo",
            "0k",
        ] {
            assert!(CopyQuality::parse(raw).is_none(), "{raw:?}");
        }
    }

    #[test]
    fn each_encoded_format_takes_its_own_rates() {
        for ok in [
            "opus:16k",
            "opus:32k-mono",
            "opus:128k",
            "mp3:64k",
            "mp3:v4-mono",
        ] {
            assert!(OutputCopy::parse(ok).is_ok(), "{ok}");
        }
        for refused in [
            "opus:v2",
            "opus:320k",
            "opus:original",
            "mp3:32k",
            "mp3:16k-mono",
        ] {
            assert!(OutputCopy::parse(refused).is_err(), "{refused}");
        }
        assert_eq!(copy("opus:32k-mono").opus_kbps(), Some((32, true)));
        assert_eq!(copy("mp3:64k").opus_kbps(), None);
        assert!(CopyFormat::Opus.is_advanced());
        assert!(!CopyFormat::M4b.is_advanced());
    }

    #[test]
    fn formats_parse_their_names_only() {
        for format in CopyFormat::ALL {
            assert_eq!(CopyFormat::parse(format.as_str()), Some(format));
            assert_eq!(CopyFormat::of_output(format.output()), Some(format));
        }
        assert_eq!(CopyFormat::parse("MP3"), Some(CopyFormat::Mp3));
        assert_eq!(CopyFormat::parse("enriched_m4b"), None);
        assert_eq!(
            CopyFormat::of_output(OutputFormat::Opus),
            Some(CopyFormat::Opus)
        );
        assert_eq!(CopyFormat::of_output(OutputFormat::SplitMp3BySize), None);
    }

    #[test]
    fn the_main_copy_follows_the_output_settings() {
        let mut lame = LameConfig::default();
        assert_eq!(
            OutputCopy::main(OutputFormat::EnrichedM4b, &lame),
            Some(copy("m4b:original"))
        );
        assert_eq!(
            OutputCopy::main(OutputFormat::None, &lame),
            Some(copy("original:original"))
        );
        assert_eq!(
            OutputCopy::main(OutputFormat::SingleMp3, &lame),
            Some(copy("mp3:v2"))
        );
        lame.target = "bitrate".into();
        lame.bitrate_kbps = 64;
        lame.mode = "mono".into();
        assert_eq!(
            OutputCopy::main(OutputFormat::SplitMp3ByChapter, &lame),
            Some(copy("mp3_chapters:64k-mono"))
        );
        assert_eq!(
            OutputCopy::main(OutputFormat::Opus, &lame),
            Some(copy("opus:64k"))
        );
        // The encoder rounds 80 kbit/s up to 96, so the main copy says 96k.
        lame.bitrate_kbps = 80;
        assert_eq!(
            OutputCopy::main(OutputFormat::SingleMp3, &lame),
            Some(copy("mp3:96k-mono"))
        );
    }

    #[test]
    fn requested_bitrates_round_up_to_the_encoders() {
        assert_eq!(mp3_cbr_kbps(0), 64);
        assert_eq!(mp3_cbr_kbps(8), 64);
        assert_eq!(mp3_cbr_kbps(64), 64);
        assert_eq!(mp3_cbr_kbps(65), 96);
        assert_eq!(mp3_cbr_kbps(257), 320);
        assert_eq!(mp3_cbr_kbps(9999), 320);
        for rate in MP3_CBR_KBPS {
            assert_eq!(mp3_cbr_kbps(u32::from(rate)), rate);
        }
    }

    #[test]
    fn a_quality_sets_the_encoder() {
        let base = LameConfig::default();
        let lame = CopyQuality::parse("64k-mono").unwrap().lame(&base);
        assert_eq!(lame.target, "bitrate");
        assert!(lame.constant_bitrate);
        assert_eq!(lame.bitrate_kbps, 64);
        assert!(lame.downsample_mono);
        assert_eq!(lame.mode, "mono");
        assert_eq!(CopyQuality::of_lame(&lame).to_string(), "64k-mono");

        let lame = CopyQuality::parse("v4").unwrap().lame(&lame);
        assert_eq!(lame.target, "quality");
        assert!(!lame.constant_bitrate);
        assert_eq!(lame.vbr_quality, 4);
        assert!(!lame.downsample_mono);
        assert_eq!(lame.mode, "default");
        assert_eq!(CopyQuality::of_lame(&lame).to_string(), "v4");

        assert_eq!(CopyQuality::ORIGINAL.lame(&base), base);
    }

    #[test]
    fn copies_parse_and_check_their_quality() {
        assert_eq!(copy("mp3:64k-mono").label(), "mp3 64k-mono");
        assert_eq!(copy(" M4B : Original ").to_string(), "m4b:original");
        for raw in [
            "mp3",
            "flac:original",
            "mp3:loud",
            "mp3:original",
            "m4b:128k",
            "original:v2",
        ] {
            assert!(OutputCopy::parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn a_copy_list_is_bounded_and_has_no_repeats() {
        let main = copy("m4b:original");
        let mp3 = copy("mp3:64k-mono");
        assert!(validate_copies(Some(&main), std::slice::from_ref(&mp3)).is_ok());
        assert!(validate_copies(Some(&main), std::slice::from_ref(&main)).is_err());
        assert!(validate_copies(Some(&main), &[mp3.clone(), mp3.clone()]).is_err());
        let many: Vec<OutputCopy> = MP3_CBR_KBPS
            .iter()
            .map(|kbps| copy(&format!("mp3:{kbps}k")))
            .chain(["mp3:v0", "mp3:v1"].map(copy))
            .collect();
        assert_eq!(many.len(), 9);
        assert!(validate_copies(None, &many).is_err());
        assert!(validate_copies(None, &many[..8]).is_ok());
    }

    #[test]
    fn the_copies_to_make_start_with_the_main_output() {
        let main = copy("m4b:original");
        let mp3 = copy("mp3:64k-mono");
        let odd = OutputCopy {
            format: CopyFormat::M4b,
            quality: CopyQuality::parse("64k").unwrap(),
        };
        assert_eq!(
            copies_to_make(
                Some(main.clone()),
                &[mp3.clone(), main.clone(), odd, mp3.clone()]
            ),
            [main.clone(), mp3.clone()]
        );
        // However long the list, at most eight extra copies are made.
        let many: Vec<OutputCopy> = MP3_CBR_KBPS
            .iter()
            .map(|kbps| copy(&format!("mp3:{kbps}k")))
            .chain(["mp3:v0", "mp3:v1", "mp3:v2"].map(copy))
            .collect();
        let made = copies_to_make(Some(main), &many);
        assert_eq!(made.len(), 1 + MAX_EXTRA_COPIES);
        assert_eq!(made[1..], many[..MAX_EXTRA_COPIES]);
        assert_eq!(copies_to_make(None, std::slice::from_ref(&mp3)), [mp3]);
    }

    #[test]
    fn copies_read_strictly_from_json() {
        let parsed: OutputCopy =
            serde_json::from_str(r#"{"format":"mp3","quality":"64k-mono"}"#).unwrap();
        assert_eq!(parsed, copy("mp3:64k-mono"));
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            r#"{"format":"mp3","quality":"64k-mono"}"#
        );
        for bad in [
            r#"{"format":"mp3","quality":"loud"}"#,
            r#"{"format":"flac","quality":"original"}"#,
            r#"{"format":"mp3","quality":"64k","extra":1}"#,
            r#"{"format":"mp3"}"#,
        ] {
            assert!(serde_json::from_str::<OutputCopy>(bad).is_err(), "{bad}");
        }
    }
}
