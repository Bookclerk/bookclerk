//! Small, valid files for tests, seeds and examples: one tagged M4B, MP3 and
//! FLAC, built in memory. Enabled by the `fixtures` feature; not part of
//! the reader.

// Builders for tests: a value that does not fit is a bug in the fixture.
#![allow(clippy::expect_used)]

use id3::frame::{Chapter, ExtendedText, Picture};
use id3::{Frame, TagLike, Version};

/// FLAC `STREAMINFO` block type.
const STREAMINFO: u8 = 0;
/// FLAC Vorbis comment block type.
const VORBIS_COMMENT: u8 = 4;
/// FLAC picture block type.
const PICTURE: u8 = 6;

/// A box of `kind` around `body`.
///
/// # Panics
///
/// Never: the fixed sizes all fit their fields.
pub fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = (u32::try_from(body.len() + 8).expect("len"))
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// A full box (version and flags) of `kind` around `body`.
fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut inner = vec![version, 0, 0, 0];
    inner.extend_from_slice(body);
    mp4_box(kind, &inner)
}

/// An `ilst` `data` box of well-known type `type_code`.
fn data(type_code: u32, value: &[u8]) -> Vec<u8> {
    let mut body = type_code.to_be_bytes().to_vec();
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(value);
    mp4_box(b"data", &body)
}

/// An MPEG-4 `ES_Descriptor` for AAC whose decoder-specific info is `asc`.
fn es_descriptor(asc: &[u8]) -> Vec<u8> {
    let asc_len = u8::try_from(asc.len()).expect("short config");
    assert!(asc_len <= 100, "config too long for the fixture");
    // Object type 0x40 (MPEG-4 audio), stream type audio, buffer size and
    // bit rates left zero, then the config.
    let mut config = vec![0x04, 13 + 2 + asc_len, 0x40, 0x15];
    config.extend_from_slice(&[0u8; 11]);
    config.extend_from_slice(&[0x05, asc_len]);
    config.extend_from_slice(asc);
    let sl = [0x06, 1, 2];
    let mut out = vec![
        0x03,
        3 + u8::try_from(config.len() + sl.len()).expect("len"),
    ];
    out.extend_from_slice(&[0, 1, 0]);
    out.extend_from_slice(&config);
    out.extend_from_slice(&sl);
    out
}

/// A QuickTime chapter title sample.
fn text_sample(title: &str) -> Vec<u8> {
    let mut out = u16::try_from(title.len())
        .expect("len")
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(title.as_bytes());
    out
}

/// An M4B: ftyp, an mdat with fake audio and two chapter samples, then
/// moov with an audio track, a text chapter track, chpl and ilst.
///
/// # Panics
///
/// Never: the fixed sizes all fit their fields.
pub fn tagged_m4b() -> Vec<u8> {
    // AAC-LC, 44.1 kHz, stereo.
    tagged_m4b_with_audio_config(&[0x12, 0x10])
}

/// [`tagged_m4b`] whose `mp4a` sample entry carries `asc` as its MPEG-4
/// `AudioSpecificConfig` (stereo, 44.1 kHz in the entry itself).
///
/// # Panics
///
/// When `asc` is longer than 100 bytes, which a one-byte descriptor length
/// cannot hold here.
pub fn tagged_m4b_with_audio_config(asc: &[u8]) -> Vec<u8> {
    let ftyp = mp4_box(b"ftyp", b"M4B \0\0\0\0M4B mp42isom");
    let samples = [text_sample("Opening"), text_sample("Chapter One")];
    let mut mdat_body = vec![0xAAu8; 4096];
    let first_sample = u32::try_from(ftyp.len() + 8 + mdat_body.len()).expect("offset");
    for sample in &samples {
        mdat_body.extend_from_slice(sample);
    }
    let mdat = mp4_box(b"mdat", &mdat_body);

    let mut mvhd = vec![0u8; 96];
    mvhd[8..12].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[12..16].copy_from_slice(&90_000u32.to_be_bytes());
    let mvhd = full(b"mvhd", 0, &mvhd);

    let tkhd = |id: u32| {
        let mut body = vec![0u8; 80];
        body[8..12].copy_from_slice(&id.to_be_bytes());
        full(b"tkhd", 0, &body)
    };
    let mdhd = |timescale: u32| {
        let mut body = vec![0u8; 20];
        body[8..12].copy_from_slice(&timescale.to_be_bytes());
        full(b"mdhd", 0, &body)
    };
    let hdlr = |handler: &[u8; 4]| {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(handler);
        body.extend_from_slice(&[0u8; 13]);
        full(b"hdlr", 0, &body)
    };

    // Audio track: an AAC sample description, then a big sample table the
    // reader must skip unread.
    let mut mp4a = vec![0u8; 28];
    // Data reference index, then after version, revision and vendor: two
    // channels, 16-bit samples, and the rate in 16.16.
    mp4a[7] = 1;
    mp4a[16..18].copy_from_slice(&2u16.to_be_bytes());
    mp4a[18..20].copy_from_slice(&16u16.to_be_bytes());
    mp4a[24..28].copy_from_slice(&(44_100u32 << 16).to_be_bytes());
    mp4a.extend_from_slice(&full(b"esds", 0, &es_descriptor(asc)));
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&mp4_box(b"mp4a", &mp4a));
    let audio_stbl = mp4_box(
        b"stbl",
        &[
            full(b"stsd", 0, &stsd),
            mp4_box(b"stsz", &vec![0u8; 200_000]),
        ]
        .concat(),
    );
    let audio_minf = mp4_box(b"minf", &audio_stbl);
    let audio_mdia = mp4_box(b"mdia", &[mdhd(44_100), hdlr(b"soun"), audio_minf].concat());
    let tref = mp4_box(b"tref", &mp4_box(b"chap", &2u32.to_be_bytes()));
    let audio = mp4_box(b"trak", &[tkhd(1), tref, audio_mdia].concat());

    // Chapter track: two samples in one chunk, 60 s apart.
    let mut stts = 1u32.to_be_bytes().to_vec();
    stts.extend_from_slice(&2u32.to_be_bytes());
    stts.extend_from_slice(&60_000u32.to_be_bytes());
    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&2u32.to_be_bytes());
    for sample in &samples {
        stsz.extend_from_slice(&u32::try_from(sample.len()).expect("len").to_be_bytes());
    }
    let mut stsc = 1u32.to_be_bytes().to_vec();
    stsc.extend_from_slice(&[1u32, 2, 1].map(u32::to_be_bytes).concat());
    let mut stco = 1u32.to_be_bytes().to_vec();
    stco.extend_from_slice(&first_sample.to_be_bytes());
    let text_stbl = mp4_box(
        b"stbl",
        &[
            full(b"stts", 0, &stts),
            full(b"stsz", 0, &stsz),
            full(b"stsc", 0, &stsc),
            full(b"stco", 0, &stco),
        ]
        .concat(),
    );
    let text_mdia = mp4_box(
        b"mdia",
        &[mdhd(1000), hdlr(b"text"), mp4_box(b"minf", &text_stbl)].concat(),
    );
    let text = mp4_box(b"trak", &[tkhd(2), text_mdia].concat());

    let mut chpl = vec![1];
    chpl.extend_from_slice(&0u64.to_be_bytes());
    chpl.push(4);
    chpl.extend_from_slice(b"Nero");
    let chpl = full(b"chpl", 0, &chpl);

    let ilst = mp4_box(
        b"ilst",
        &[
            mp4_box(b"\xA9nam", &data(1, b"The Book")),
            mp4_box(b"\xA9ART", &data(1, b"An Author")),
            mp4_box(b"trkn", &data(0, &[0, 0, 0, 3, 0, 9, 0, 0])),
            mp4_box(
                b"----",
                &[
                    full(b"mean", 0, b"com.pilabor.tone"),
                    full(b"name", 0, b"AUDIBLE_ASIN"),
                    data(1, b"B000000001"),
                ]
                .concat(),
            ),
            mp4_box(b"covr", &data(13, &[0xFF, 0xD8, 0xFF, 0xDB, 9, 9])),
            mp4_box(b"tmpo", &data(21, &[0xFF, 0xFE])),
        ]
        .concat(),
    );
    let meta = full(b"meta", 0, &[hdlr(b"mdir"), ilst].concat());
    let udta = mp4_box(b"udta", &[chpl, meta].concat());
    let moov = mp4_box(b"moov", &[mvhd, audio, text, udta].concat());
    [ftyp, mdat, moov].concat()
}

/// A tagged MP3: an ID3v2.4 tag and a few silent frames.
///
/// # Panics
///
/// Never: the fixed tag always encodes.
pub fn tagged_mp3() -> Vec<u8> {
    let mut tag = id3::Tag::new();
    tag.set_title("The Book");
    tag.set_artist("An Author");
    tag.set_album("The Book");
    tag.add_frame(Frame::text("TCOM", "A Narrator"));
    tag.add_frame(Frame::text("TCON", "Fantasy\0Epic"));
    tag.add_frame(ExtendedText {
        description: "ASIN".into(),
        value: "B000000001".into(),
    });
    tag.add_frame(Picture {
        mime_type: "image/jpeg".into(),
        picture_type: id3::frame::PictureType::CoverFront,
        description: String::new(),
        data: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4],
    });
    for (i, (title, start)) in [("Opening", 0u32), ("One", 1500)].iter().enumerate() {
        tag.add_frame(Chapter {
            element_id: format!("ch{i}"),
            start_time: *start,
            end_time: start + 1500,
            start_offset: u32::MAX,
            end_offset: u32::MAX,
            frames: vec![Frame::text("TIT2", *title)],
        });
    }
    let mut out = Vec::new();
    tag.write_to(&mut out, Version::Id3v24).expect("write tag");
    let header = [0xFF, 0xFB, 0x90, 0x00];
    for _ in 0..8 {
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&header);
        out.extend(frame);
    }
    out
}

/// A FLAC metadata block.
fn block(kind: u8, last: bool, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("len").to_be_bytes();
    let mut out = vec![kind | if last { 0x80 } else { 0 }, len[1], len[2], len[3]];
    out.extend_from_slice(body);
    out
}

/// A Vorbis comment block body (vendor `vendor`) holding `items`.
///
/// # Panics
///
/// When an item is longer than `u32::MAX` bytes.
#[must_use]
pub fn vorbis_comments(items: &[&str]) -> Vec<u8> {
    comments(items)
}

/// The body of [`vorbis_comments`].
fn comments(items: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<u8>, s: &[u8]| {
        out.extend_from_slice(&u32::try_from(s.len()).expect("len").to_le_bytes());
        out.extend_from_slice(s);
    };
    push(&mut out, b"vendor");
    out.extend_from_slice(&u32::try_from(items.len()).expect("len").to_le_bytes());
    for item in items {
        push(&mut out, item.as_bytes());
    }
    out
}

/// A FLAC file: 10 s at 44.1 kHz, three comments, two chapters, a cover.
///
/// # Panics
///
/// Never: the fixed sizes all fit their fields.
pub fn tagged_flac() -> Vec<u8> {
    let mut streaminfo = vec![0u8; 34];
    let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 441_000;
    streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
    let mut pic = Vec::new();
    pic.extend_from_slice(&3u32.to_be_bytes());
    pic.extend_from_slice(&9u32.to_be_bytes());
    pic.extend_from_slice(b"image/png");
    pic.extend_from_slice(&0u32.to_be_bytes());
    pic.extend_from_slice(&[0u8; 16]);
    let png = b"\x89PNG\r\n\x1a\nrest";
    pic.extend_from_slice(&u32::try_from(png.len()).expect("len").to_be_bytes());
    pic.extend_from_slice(png);
    let mut file = b"fLaC".to_vec();
    file.extend(block(STREAMINFO, false, &streaminfo));
    file.extend(block(1, false, &[0u8; 64]));
    file.extend(block(
        VORBIS_COMMENT,
        false,
        &comments(&[
            "TITLE=The Book",
            "artist=An Author",
            "NARRATOR=Someone",
            "CHAPTER002=00:01:00.5",
            "CHAPTER001=00:00:00.000",
            "CHAPTER001NAME=Start",
            "CHAPTER002NAME=Later",
            "no equals sign",
        ]),
    ));
    file.extend(block(PICTURE, true, &pic));
    file.extend_from_slice(&[0xFF, 0xF8, 0, 0, 0, 0]);
    file
}

/// One Ogg page of stream `serial` with header flags `flags` and granule
/// position `granule`, holding `packets` laced in order. A packet whose
/// length is a multiple of 255 ends with a zero lacing value unless it is
/// the page's last and exactly fills the table (so it continues).
///
/// # Panics
///
/// When the packets need more than 255 lacing values.
#[must_use]
pub fn ogg_page(flags: u8, granule: u64, serial: u32, packets: &[&[u8]]) -> Vec<u8> {
    let mut lacing = Vec::new();
    for (index, packet) in packets.iter().enumerate() {
        let full = packet.len() / 255;
        lacing.extend(std::iter::repeat_n(255u8, full));
        let last = index + 1 == packets.len();
        let fills = last && packet.len() % 255 == 0 && lacing.len() == 255;
        if !fills {
            lacing.push(u8::try_from(packet.len() % 255).expect("remainder"));
        }
    }
    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes());
    page.push(u8::try_from(lacing.len()).expect("at most 255 lacing values"));
    page.extend_from_slice(&lacing);
    for packet in packets {
        page.extend_from_slice(packet);
    }
    let crc = crate::ogg::page_crc(&page[..22], &page[26..]);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let index = usize::try_from((n >> (18 - 6 * i)) & 63).expect("six bits");
                out.push(char::from(DIGITS[index]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An Ogg Opus file: 10 s of (silent, empty) audio past a 312-sample
/// pre-skip, a title, two chapters and a PNG cover as a
/// `METADATA_BLOCK_PICTURE` comment.
///
/// # Panics
///
/// Never: the fixed sizes all fit their fields.
#[must_use]
pub fn tagged_ogg_opus() -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    let mut pic = Vec::new();
    pic.extend_from_slice(&3u32.to_be_bytes());
    pic.extend_from_slice(&9u32.to_be_bytes());
    pic.extend_from_slice(b"image/png");
    pic.extend_from_slice(&0u32.to_be_bytes());
    pic.extend_from_slice(&[0u8; 16]);
    let png = b"\x89PNG\r\n\x1a\nrest";
    pic.extend_from_slice(&u32::try_from(png.len()).expect("len").to_be_bytes());
    pic.extend_from_slice(png);
    let picture = format!("METADATA_BLOCK_PICTURE={}", base64(&pic));
    let mut tags = b"OpusTags".to_vec();
    tags.extend(comments(&[
        "TITLE=The Book",
        "ARTIST=An Author",
        "CHAPTER001=00:00:00.000",
        "CHAPTER001NAME=Start",
        "CHAPTER002=00:00:05.000",
        "CHAPTER002NAME=Later",
        &picture,
    ]));
    let mut file = ogg_page(0x02, 0, 0x4243_4C4B, &[&head]);
    file.extend(ogg_page(0, 0, 0x4243_4C4B, &[&tags]));
    // One silent packet (TOC byte only) on the last page.
    file.extend(ogg_page(0x04, 312 + 480_000, 0x4243_4C4B, &[&[0xF8]]));
    file
}
