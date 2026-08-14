//! Synthetic progressive MP4 files for tests.
//!
//! Behind the `fixtures` feature so it never reaches a release build. Tests in
//! this crate and in the store plugins both need real files to parse — a plugin
//! proving its decrypt round-trips through [`crate::remux_progressive`] needs
//! one that carries its sample entry — and a writer they share is one fewer
//! copy of ISO-BMFF layout to keep correct.

use std::io::Write;
use std::path::Path;

use crate::error::Result;

/// A progressive MP4 with one AAC audio track, described by its samples.
#[derive(Debug, Clone)]
pub struct ProgressiveFixture {
    /// Media timescale (ticks per second) of the audio track.
    pub timescale: u32,
    /// Duration of every sample, in timescale ticks.
    pub sample_duration: u32,
    /// Payload bytes of each sample, in track order.
    pub samples: Vec<Vec<u8>>,
    /// `stsd` sample entry type: `mp4a` for clear AAC, `aavd` for Audible's
    /// encrypted flavour, or anything else a reader should cope with.
    pub sample_entry: [u8; 4],
    /// How many samples each chunk holds. Must divide the sample count.
    pub samples_per_chunk: u32,
}

impl Default for ProgressiveFixture {
    fn default() -> Self {
        Self {
            timescale: 44_100,
            sample_duration: 1024,
            samples: Vec::new(),
            sample_entry: *b"mp4a",
            samples_per_chunk: 1,
        }
    }
}

impl ProgressiveFixture {
    /// A fixture of `count` samples whose bytes are deterministic but distinct.
    ///
    /// # Arguments
    ///
    /// * `count` - Numeric `count` value for this call.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    #[must_use]
    pub fn with_generated_samples(count: usize) -> Self {
        let samples = (0..count)
            .map(|i| {
                // Sizes vary so a rebuilt stsz cannot accidentally pass with a
                // single size, and bytes vary so a misordered copy is visible.
                let len = 32 + (i % 7) * 8;
                (0..len).map(|b| (i as u8).wrapping_add(b as u8)).collect()
            })
            .collect();
        Self {
            samples,
            ..Self::default()
        }
    }

    /// Replace every sample payload, keeping the timing.
    ///
    /// # Arguments
    ///
    /// * `samples` - `samples` input for this call.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    #[must_use]
    pub fn with_samples(mut self, samples: Vec<Vec<u8>>) -> Self {
        self.samples = samples;
        self
    }

    /// Set the `stsd` sample entry type.
    ///
    /// # Arguments
    ///
    /// * `sample_entry` - `sample_entry` input for this call.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    #[must_use]
    pub fn with_sample_entry(mut self, sample_entry: &[u8; 4]) -> Self {
        self.sample_entry = *sample_entry;
        self
    }

    /// Duration of the whole track in milliseconds.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        if self.timescale == 0 {
            return 0;
        }
        let ticks = u64::from(self.sample_duration) * self.samples.len() as u64;
        ticks * 1000 / u64::from(self.timescale)
    }

    /// Write the file. Payloads land in a single trailing `mdat`.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem path involved in this operation.
    ///
    /// # Returns
    ///
    /// The successful result value for this operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying I/O, parse, network, or store operation fails.
    ///
    /// # Panics
    ///
    /// Panics when an internal invariant does not hold.
    pub fn write(&self, path: &Path) -> Result<()> {
        assert!(
            !self.samples.is_empty(),
            "fixture needs at least one sample"
        );
        assert!(self.samples_per_chunk > 0, "samples_per_chunk must be > 0");
        assert_eq!(
            self.samples.len() % self.samples_per_chunk as usize,
            0,
            "samples_per_chunk must divide the sample count"
        );

        let sizes: Vec<u32> = self
            .samples
            .iter()
            .map(|s| u32::try_from(s.len()).expect("sample fits in u32"))
            .collect();
        let chunk_count = self.samples.len() / self.samples_per_chunk as usize;

        let ftyp = boxed(
            b"ftyp",
            &concat(&[b"isom", &0u32.to_be_bytes(), b"isom", b"iso2", b"mp41"]),
        );

        // moov length is independent of the offset *values*, so build it once
        // with placeholders to learn the payload start, then again for real.
        let placeholder = vec![0u32; chunk_count];
        let sized_moov = self.build_moov(&sizes, &placeholder);
        let mdat_payload_start = (ftyp.len() + sized_moov.len() + 8) as u32;

        let mut chunk_offsets = Vec::with_capacity(chunk_count);
        let mut offset = mdat_payload_start;
        for chunk in self.samples.chunks(self.samples_per_chunk as usize) {
            chunk_offsets.push(offset);
            offset += chunk.iter().map(|s| s.len() as u32).sum::<u32>();
        }
        let moov = self.build_moov(&sizes, &chunk_offsets);
        assert_eq!(moov.len(), sized_moov.len(), "moov length must be stable");

        let payload_len: usize = self.samples.iter().map(Vec::len).sum();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(path)?;
        out.write_all(&ftyp)?;
        out.write_all(&moov)?;
        out.write_all(&((payload_len + 8) as u32).to_be_bytes())?;
        out.write_all(b"mdat")?;
        for sample in &self.samples {
            out.write_all(sample)?;
        }
        out.sync_all()?;
        Ok(())
    }

    /// Builds a minimal movie box (one `soun` track) for fixture MP4s used in tests.
    fn build_moov(&self, sizes: &[u32], chunk_offsets: &[u32]) -> Vec<u8> {
        let media_duration = u64::from(self.sample_duration) * self.samples.len() as u64;
        let movie_timescale = 1000u32;
        let movie_duration = self.duration_ms();

        let mvhd = boxed(
            b"mvhd",
            &concat(&[
                &0u32.to_be_bytes(),                    // version 0 + flags
                &0u32.to_be_bytes(),                    // creation time
                &0u32.to_be_bytes(),                    // modification time
                &movie_timescale.to_be_bytes(),         // timescale
                &(movie_duration as u32).to_be_bytes(), // duration
                &0x0001_0000u32.to_be_bytes(),          // rate 1.0
                &0x0100u16.to_be_bytes(),               // volume 1.0
                &[0u8; 2],                              // reserved
                &[0u8; 8],                              // reserved
                &UNITY_MATRIX,                          // matrix
                &[0u8; 24],                             // pre_defined
                &2u32.to_be_bytes(),                    // next track id
            ]),
        );

        let tkhd = boxed(
            b"tkhd",
            &concat(&[
                &0x0000_0007u32.to_be_bytes(),          // version 0, enabled
                &0u32.to_be_bytes(),                    // creation time
                &0u32.to_be_bytes(),                    // modification time
                &1u32.to_be_bytes(),                    // track id
                &0u32.to_be_bytes(),                    // reserved
                &(movie_duration as u32).to_be_bytes(), // duration
                &[0u8; 8],                              // reserved
                &0u16.to_be_bytes(),                    // layer
                &0u16.to_be_bytes(),                    // alternate group
                &0x0100u16.to_be_bytes(),               // volume 1.0
                &[0u8; 2],                              // reserved
                &UNITY_MATRIX,
                &0u32.to_be_bytes(), // width
                &0u32.to_be_bytes(), // height
            ]),
        );

        let mdhd = boxed(
            b"mdhd",
            &concat(&[
                &0u32.to_be_bytes(),                    // version 0 + flags
                &0u32.to_be_bytes(),                    // creation time
                &0u32.to_be_bytes(),                    // modification time
                &self.timescale.to_be_bytes(),          // timescale
                &(media_duration as u32).to_be_bytes(), // duration
                &0x55c4u16.to_be_bytes(),               // language "und"
                &0u16.to_be_bytes(),                    // pre_defined
            ]),
        );

        let hdlr = boxed(
            b"hdlr",
            &concat(&[
                &0u32.to_be_bytes(), // version + flags
                &0u32.to_be_bytes(), // pre_defined
                b"soun",             // handler type
                &[0u8; 12],          // reserved
                &[0u8; 1],           // empty name
            ]),
        );

        let smhd = boxed(
            b"smhd",
            &concat(&[
                &0u32.to_be_bytes(),
                &0u16.to_be_bytes(),
                &0u16.to_be_bytes(),
            ]),
        );
        // FullBox with flags = 1: the media lives in this same file.
        let url = boxed(b"url ", &0x0000_0001u32.to_be_bytes());
        let dref = boxed(
            b"dref",
            &concat(&[&0u32.to_be_bytes(), &1u32.to_be_bytes(), &url]),
        );
        let dinf = boxed(b"dinf", &dref);

        let stsd = boxed(
            b"stsd",
            &concat(&[
                &0u32.to_be_bytes(), // version + flags
                &1u32.to_be_bytes(), // entry count
                &self.sample_entry_box(),
            ]),
        );
        let stts = boxed(
            b"stts",
            &concat(&[
                &0u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &(sizes.len() as u32).to_be_bytes(),
                &self.sample_duration.to_be_bytes(),
            ]),
        );
        let stsc = boxed(
            b"stsc",
            &concat(&[
                &0u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &1u32.to_be_bytes(), // first chunk
                &self.samples_per_chunk.to_be_bytes(),
                &1u32.to_be_bytes(), // sample description index
            ]),
        );
        let mut stsz_body = concat(&[
            &0u32.to_be_bytes(),
            &0u32.to_be_bytes(), // per-sample sizes follow
            &(sizes.len() as u32).to_be_bytes(),
        ]);
        for size in sizes {
            stsz_body.extend_from_slice(&size.to_be_bytes());
        }
        let stsz = boxed(b"stsz", &stsz_body);
        let mut stco_body = concat(&[
            &0u32.to_be_bytes(),
            &(chunk_offsets.len() as u32).to_be_bytes(),
        ]);
        for offset in chunk_offsets {
            stco_body.extend_from_slice(&offset.to_be_bytes());
        }
        let stco = boxed(b"stco", &stco_body);

        let stbl = boxed(b"stbl", &concat(&[&stsd, &stts, &stsc, &stsz, &stco]));
        let minf = boxed(b"minf", &concat(&[&smhd, &dinf, &stbl]));
        let mdia = boxed(b"mdia", &concat(&[&mdhd, &hdlr, &minf]));
        let trak = boxed(b"trak", &concat(&[&tkhd, &mdia]));
        boxed(b"moov", &concat(&[&mvhd, &trak]))
    }

    /// An `AudioSampleEntry` with an `esds` carrying a stereo 44.1 kHz ASC.
    fn sample_entry_box(&self) -> Vec<u8> {
        // ES_Descriptor (0x03) → DecoderConfigDescriptor (0x04) → ASC (0x05).
        let asc: [u8; 2] = [0x12, 0x10]; // AAC LC, 44.1 kHz, stereo
        let dsi = concat(&[&[0x05u8, asc.len() as u8], &asc]);
        let dcd_body = concat(&[
            &[0x40u8, 0x15], // MPEG-4 audio, stream type
            &[0u8; 3],       // buffer size
            &[0u8; 8],       // max + avg bitrate
            &dsi,
        ]);
        let dcd = concat(&[&[0x04u8, dcd_body.len() as u8], &dcd_body]);
        let sl = [0x06u8, 0x01, 0x02];
        let es_body = concat(&[&1u16.to_be_bytes(), &[0u8], &dcd, &sl]);
        let es = concat(&[&[0x03u8, es_body.len() as u8], &es_body]);
        let esds = boxed(b"esds", &concat(&[&0u32.to_be_bytes(), &es]));

        let body = concat(&[
            &[0u8; 6],                        // reserved
            &1u16.to_be_bytes(),              // data reference index
            &[0u8; 8],                        // version / revision / vendor
            &2u16.to_be_bytes(),              // channel count
            &16u16.to_be_bytes(),             // sample size
            &0u16.to_be_bytes(),              // pre_defined
            &0u16.to_be_bytes(),              // reserved
            &(44_100u32 << 16).to_be_bytes(), // sample rate 16.16
            &esds,
        ]);
        boxed(&self.sample_entry, &body)
    }
}

/// Identity 3×3 transformation matrix written into fixture `mvhd` / `tkhd` atoms.
const UNITY_MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0x00, 0x00, 0x00,
];

/// Prefixes a 32-bit size and fourcc onto a fixture atom body.
fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// Concatenates byte slices into one fixture atom payload.
fn concat(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}
