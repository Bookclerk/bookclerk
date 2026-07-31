//! Round-trip a synthetic progressive MP4 through the remuxer.
//!
//! These cover what both callers depend on: payloads come out in order and
//! unchanged, a transform sees each payload exactly once and can rewrite it, a
//! trim keeps the right window, and per-sample state a caller holds can be
//! lined up with the samples that survived the trim.

use std::path::Path;

use bookclerk_mp4::fixture::ProgressiveFixture;
use bookclerk_mp4::{
    parse_mp4, read_moov, remux_progressive, strip_trailing_free, track_duration_ms,
    write_moov_in_place, CopySamples, Mp4Error, RemuxOptions, SampleEntryKind, SampleTransform,
    TrimRange, RESERVED_MOOV_SLACK,
};

/// Read every sample payload back out of a progressive file.
fn payloads(path: &Path) -> Vec<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mp4 = parse_mp4(path).expect("parse remuxed file");
    let mut file = std::fs::File::open(path).expect("open remuxed file");
    mp4.audio
        .samples
        .iter()
        .map(|sample| {
            let mut buf = vec![0u8; sample.size as usize];
            file.seek(SeekFrom::Start(sample.offset)).expect("seek");
            file.read_exact(&mut buf).expect("read sample");
            buf
        })
        .collect()
}

/// Tags, a cover and a chapter track all arrive after the media is written, and
/// all of them live in `moov`. The remuxer leaves them room, so adding them
/// later is a header-sized write rather than a rewrite of the book.
#[test]
fn an_edit_that_fits_the_reserved_slack_leaves_the_media_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    let output = dir.path().join("out.m4b");
    ProgressiveFixture::default()
        .with_samples((0..64).map(|i| vec![i as u8; 128]).collect())
        .write(&input)
        .unwrap();
    remux_progressive(&input, &output, &RemuxOptions::default(), &mut CopySamples).unwrap();

    let before = payloads(&output);
    let size_before = std::fs::metadata(&output).unwrap().len();

    let (at, mut moov) = read_moov(&output).unwrap();
    assert!(
        !at.is_last,
        "a faststart file puts moov in front of the media"
    );
    assert!(
        strip_trailing_free(&mut moov).unwrap(),
        "the remuxer must reserve slack"
    );
    assert!(
        at.len as usize - moov.len() >= RESERVED_MOOV_SLACK,
        "reserved {} bytes, wanted at least {RESERVED_MOOV_SLACK}",
        at.len as usize - moov.len()
    );

    // Stand in for a tag set with a cover: big enough to be worth reserving for.
    let cover = vec![0xAB; 256 * 1024];
    moov.extend_from_slice(&((cover.len() + 8) as u32).to_be_bytes());
    moov.extend_from_slice(b"udta");
    moov.extend_from_slice(&cover);
    let grown = moov.len() as u32;
    moov[0..4].copy_from_slice(&grown.to_be_bytes());

    write_moov_in_place(&output, at, &moov).unwrap();

    assert_eq!(
        std::fs::metadata(&output).unwrap().len(),
        size_before,
        "the file must be exactly as long as it was"
    );
    assert_eq!(payloads(&output), before, "the media must not have moved");
}

#[test]
fn a_copied_remux_preserves_every_payload_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    let output = dir.path().join("out.m4b");

    // More than one sample per chunk, so the rebuilt one-per-chunk table has to
    // recompute offsets rather than copy them.
    let fixture = ProgressiveFixture {
        samples_per_chunk: 4,
        ..ProgressiveFixture::with_generated_samples(40)
    };
    fixture.write(&input).unwrap();

    remux_progressive(&input, &output, &RemuxOptions::default(), &mut CopySamples).unwrap();

    assert_eq!(payloads(&output), fixture.samples);

    let out = parse_mp4(&output).unwrap();
    assert_eq!(out.major_brand.as_str(), "M4B ");
    assert_eq!(out.audio.timescale, fixture.timescale);
    assert_eq!(track_duration_ms(&out.audio), fixture.duration_ms());
}

#[test]
fn a_trim_keeps_only_the_requested_window() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    let output = dir.path().join("out.m4b");

    // 1000 ticks per second and 100 ticks per sample → one sample per 100 ms.
    let fixture = ProgressiveFixture {
        timescale: 1000,
        sample_duration: 100,
        ..ProgressiveFixture::with_generated_samples(20)
    };
    fixture.write(&input).unwrap();

    remux_progressive(
        &input,
        &output,
        &RemuxOptions {
            trim: Some(TrimRange {
                start_ms: 500,
                end_ms: Some(1500),
            }),
        },
        &mut CopySamples,
    )
    .unwrap();

    assert_eq!(payloads(&output), fixture.samples[5..15]);

    // Durations follow the kept samples, not the original track.
    let out = parse_mp4(&output).unwrap();
    assert_eq!(track_duration_ms(&out.audio), 1000);
}

#[test]
fn a_trim_that_keeps_nothing_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    let fixture = ProgressiveFixture {
        timescale: 1000,
        sample_duration: 100,
        ..ProgressiveFixture::with_generated_samples(10)
    };
    fixture.write(&input).unwrap();

    let err = remux_progressive(
        &input,
        &dir.path().join("out.m4b"),
        &RemuxOptions {
            trim: Some(TrimRange {
                start_ms: 5_000,
                end_ms: None,
            }),
        },
        &mut CopySamples,
    )
    .unwrap_err();
    assert!(
        matches!(&err, Mp4Error::Container(detail) if detail.contains("no samples remain")),
        "{err}"
    );
}

#[test]
fn a_trim_of_a_file_larger_than_the_read_buffer_still_copies_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    let output = dir.path().join("out.m4b");

    // Payloads well past the remuxer's read buffer, so the copy has to refill it
    // repeatedly, and a trim start that lands beyond it, so the first read has to
    // seek rather than skip forward through buffered bytes.
    let fixture = ProgressiveFixture {
        timescale: 1000,
        sample_duration: 10,
        samples_per_chunk: 8,
        ..ProgressiveFixture::default()
    }
    .with_samples((0..4_000).map(|i| vec![(i % 251) as u8; 400]).collect());
    assert!(
        fixture.samples.iter().map(Vec::len).sum::<usize>() > 1 << 20,
        "fixture must be bigger than the read buffer to be worth testing"
    );
    fixture.write(&input).unwrap();

    remux_progressive(
        &input,
        &output,
        &RemuxOptions {
            trim: Some(TrimRange {
                start_ms: 5_000,
                end_ms: Some(35_000),
            }),
        },
        &mut CopySamples,
    )
    .unwrap();

    assert_eq!(payloads(&output), fixture.samples[500..3_500]);
}

/// Stands in for a decrypting plugin: one keystream byte per original sample,
/// XORed over the payload. Trivial, but it exercises the same contract — state
/// indexed by original sample, narrowed to the kept window, applied in place.
struct XorBySample {
    keys: Vec<u8>,
    applied: Vec<usize>,
}

impl SampleTransform for XorBySample {
    fn retain(&mut self, kept: &[usize]) -> bookclerk_mp4::Result<()> {
        self.keys = kept.iter().map(|&i| self.keys[i]).collect();
        Ok(())
    }

    fn sample(&mut self, index: usize, payload: &mut [u8]) -> bookclerk_mp4::Result<()> {
        let key = *self
            .keys
            .get(index)
            .ok_or_else(|| Mp4Error::transform(format!("no key for sample {index}")))?;
        for byte in payload.iter_mut() {
            *byte ^= key;
        }
        self.applied.push(index);
        Ok(())
    }
}

#[test]
fn a_transform_rewrites_each_kept_sample_once_with_its_own_state() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    let output = dir.path().join("out.m4b");

    let fixture = ProgressiveFixture {
        timescale: 1000,
        sample_duration: 100,
        samples_per_chunk: 2,
        // An `aavd` entry stands in for protected content; the remuxer must
        // publish the result as clear `mp4a`.
        ..ProgressiveFixture::with_generated_samples(20).with_sample_entry(b"aavd")
    };
    fixture.write(&input).unwrap();
    assert_eq!(
        parse_mp4(&input).unwrap().audio.sample_entry_kind,
        SampleEntryKind::Aavd
    );

    let keys: Vec<u8> = (0..fixture.samples.len())
        .map(|i| 0x5a ^ (i as u8))
        .collect();
    let mut transform = XorBySample {
        keys: keys.clone(),
        applied: Vec::new(),
    };

    remux_progressive(
        &input,
        &output,
        &RemuxOptions {
            trim: Some(TrimRange {
                start_ms: 400,
                end_ms: Some(1_000),
            }),
        },
        &mut transform,
    )
    .unwrap();

    // Every kept sample was handed over exactly once, in output order.
    assert_eq!(transform.applied, (0..6).collect::<Vec<_>>());

    // Each payload was rewritten with the key belonging to its *original*
    // sample, which is what a per-sample IV table needs from `retain`.
    let expected: Vec<Vec<u8>> = (4..10)
        .map(|i| {
            fixture.samples[i]
                .iter()
                .map(|byte| byte ^ keys[i])
                .collect()
        })
        .collect();
    assert_eq!(payloads(&output), expected);

    assert_eq!(
        parse_mp4(&output).unwrap().audio.sample_entry_kind,
        SampleEntryKind::Mp4a,
        "a remuxed file must not still claim to be protected"
    );
}

#[test]
fn a_transform_failure_stops_the_remux() {
    struct Refuse;
    impl SampleTransform for Refuse {
        fn sample(&mut self, index: usize, _payload: &mut [u8]) -> bookclerk_mp4::Result<()> {
            Err(Mp4Error::transform(format!("refusing sample {index}")))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    ProgressiveFixture::with_generated_samples(4)
        .write(&input)
        .unwrap();

    let err = remux_progressive(
        &input,
        &dir.path().join("out.m4b"),
        &RemuxOptions::default(),
        &mut Refuse,
    )
    .unwrap_err();
    assert!(
        matches!(&err, Mp4Error::Transform(detail) if detail.contains("refusing sample 0")),
        "{err}"
    );
}

#[test]
fn a_clear_sample_entry_yields_a_readable_decoder_config() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.m4a");
    ProgressiveFixture::with_generated_samples(4)
        .write(&input)
        .unwrap();

    let mp4 = parse_mp4(&input).unwrap();
    let config = bookclerk_mp4::extract_mp4a_config(&mp4).unwrap();
    assert_eq!(config.sample_rate, 44_100);
    assert_eq!(config.channels, 2);
    assert_eq!(config.asc, vec![0x12, 0x10]);
}
