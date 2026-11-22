//! Tests for the demuxer and decoder over the fixtures.

// Test code: a fixture that does not decode is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;

use super::*;

/// Demuxes and decodes `bytes` to the end.
///
/// # Returns
///
/// The track, the total frames decoded, the decoded rate and channel
/// count, and the peak absolute sample.
fn decode_all(bytes: &[u8]) -> (Track, usize, u32, usize, f32) {
    let mut demuxer = Demuxer::open(Box::new(Cursor::new(bytes.to_vec()))).expect("open");
    let track = demuxer.track().clone();
    let mut decoder = Decoder::new(&track.config).expect("decoder");
    let (mut frames, mut rate, mut channels, mut peak) = (0, 0, 0, 0f32);
    while let Some(packet) = demuxer.next_packet().expect("packet") {
        decoder
            .decode(packet.data, |frame| {
                frames += frame.frames;
                rate = frame.sample_rate;
                channels = frame.channels;
                peak = frame
                    .samples
                    .iter()
                    .fold(peak, |peak, sample| peak.max(sample.abs()));
                assert_eq!(frame.samples.len(), frame.frames * frame.channels);
                let mask = frame.channel_mask;
                assert!(mask == 0 || mask.count_ones() as usize == frame.channels);
            })
            .expect("decode");
    }
    (track, frames, rate, channels, peak)
}

#[test]
fn every_fixture_decodes_to_about_half_a_second() {
    let amr_nb = fixtures::amr_nb();
    let amr_wb = fixtures::amr_wb();
    for (bytes, codec, rate, channels) in [
        (fixtures::WMAV2, Codec::WmaV2, 44_100, 2),
        (fixtures::WMAV1, Codec::WmaV1, 44_100, 1),
        (fixtures::AC3, Codec::Ac3, 48_000, 2),
        (fixtures::EAC3, Codec::Eac3, 48_000, 2),
        (fixtures::EAC3_MP4, Codec::Eac3, 48_000, 2),
        (&amr_nb[..], Codec::AmrNb, 8_000, 1),
        (&amr_wb[..], Codec::AmrWb, 16_000, 1),
        (fixtures::AMR_WB_3GP, Codec::AmrWb, 16_000, 1),
    ] {
        let (track, frames, got_rate, got_channels, peak) = decode_all(bytes);
        assert_eq!(track.config.codec, codec);
        assert_eq!((got_rate, got_channels), (rate, channels), "{codec:?}");
        let seconds = frames as f64 / f64::from(rate);
        assert!((0.4..0.7).contains(&seconds), "{codec:?}: {seconds} s");
        assert!(peak > 0.05 && peak <= 1.0, "{codec:?}: peak {peak}");
    }
}

#[test]
fn sniff_finds_only_the_containers_this_crate_reads() {
    assert!(sniff(fixtures::WMAV2));
    assert!(sniff(fixtures::AC3));
    assert!(sniff(fixtures::EAC3));
    assert!(sniff(&fixtures::amr_nb()));
    assert!(sniff(&fixtures::amr_wb()));
    // MP4 goes to the other demuxer first.
    assert!(!sniff(fixtures::EAC3_MP4));
    assert!(!sniff(b"ID3\x04"));
    assert!(!sniff(b""));
}

#[test]
fn garbage_is_refused_without_panicking() {
    for bytes in [
        Vec::new(),
        b"not audio at all".to_vec(),
        vec![0u8; 4096],
        ASF_MAGIC.to_vec(),
        fixtures::WMAV2[..200].to_vec(),
    ] {
        if let Ok(mut demuxer) = Demuxer::open(Box::new(Cursor::new(bytes))) {
            while let Ok(Some(_)) = demuxer.next_packet() {}
        }
    }
    // A decoder fed noise rejects packets rather than crashing.
    let mut decoder = Decoder::new(&CodecConfig::bare(Codec::Ac3)).expect("decoder");
    for packet in [&[0x0B, 0x77, 0xFF][..], &[0x5A; 3000][..]] {
        let _ = decoder.decode(packet, |_| {});
    }
    assert_eq!(decoder.decode(&[], |_| {}), Err(Error::Decode));
    decoder.flush();
}

#[test]
fn wma_without_its_setup_bytes_is_refused() {
    // WMA cannot open without block_align and the container's setup data.
    assert!(Decoder::new(&CodecConfig::bare(Codec::WmaV2)).is_err());
    let mut config = CodecConfig::bare(Codec::WmaV2);
    config.extradata = vec![0; MAX_EXTRADATA + 1];
    assert_eq!(Decoder::new(&config).err(), Some(Error::Arguments));
}

#[test]
fn a_seek_lands_before_the_target() {
    let mut demuxer = Demuxer::open(Box::new(Cursor::new(fixtures::AC3.to_vec()))).expect("open");
    let (num, den) = demuxer.track().time_base;
    // A quarter second in, in track ticks.
    let target = i64::from(den) / (4 * i64::from(num));
    demuxer.seek(target).expect("seek");
    let packet = demuxer.next_packet().expect("read").expect("a packet");
    assert!(packet.pts <= target, "{} > {target}", packet.pts);
    assert!(packet.pts > 0);
}

#[test]
fn codec_numbers_round_trip() {
    for codec in Codec::ALL {
        assert_eq!(Codec::from_number(codec.number()), Some(codec));
        assert!(!codec.name().is_empty());
    }
    assert_eq!(Codec::from_number(0), None);
    assert_eq!(Codec::from_number(9), None);
}

#[test]
fn a_failing_source_is_a_demux_error() {
    /// A source whose every read and seek fails.
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::Other.into())
        }
    }
    impl std::io::Seek for Broken {
        fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
            Err(std::io::ErrorKind::Other.into())
        }
    }
    assert!(Demuxer::open(Box::new(Broken)).is_err());
}
