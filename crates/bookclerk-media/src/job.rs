//! The unit of work handed to a media worker process.
//!
//! One [`MediaJob`] is one codec operation. The job carries every path it will
//! touch, which lets the worker build its own filesystem allowlist before doing
//! anything: the jail is derived from the request rather than configured
//! separately, so a job can never be granted more than it declared.

use std::path::{Path, PathBuf};

use bookclerk_config::LameConfig;
use serde::{Deserialize, Serialize};

use crate::chapter_align::ChapterAlignOptions;
use crate::error::Result;
use crate::metadata::FixupRequest;
use crate::package_m4b::PackageM4bRequest;
use crate::MediaOutcome;
use bookclerk_mp4::TrimRange;

/// A single codec operation.
///
/// These are the CPU-bound paths that decode or encode attacker-influenced
/// audio through LAME and FDK-AAC, both of which are C libraries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MediaJob {
    /// Re-encode audio to MP3 via Symphonia and LAME.
    EncodeMp3 {
        input: PathBuf,
        output: PathBuf,
        lame: Box<LameConfig>,
        max_sample_rate: Option<u32>,
    },
    /// Copy or trim a progressive M4B/M4A into a new file.
    RemuxTrimmed {
        input: PathBuf,
        output: PathBuf,
        trim: TrimRange,
    },
    /// Write metadata tags, cover art, and chapters.
    Fixup { request: Box<FixupRequest> },
    /// Package ordered audio parts into one M4B.
    PackageM4b { request: Box<PackageM4bRequest> },
    /// Snap chapter starts to spoken-title onsets by local waveform analysis.
    AlignChapters {
        path: PathBuf,
        chapters: Vec<(String, u64)>,
        options: ChapterAlignOptions,
    },
}

/// What a finished job produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum MediaJobOutput {
    /// A single written file.
    File { output: PathBuf },
    /// A written file plus the chapter list that was muxed into it.
    FileWithChapters {
        output: PathBuf,
        chapters: Vec<(String, u64)>,
    },
    /// Adjusted chapter starts; writes nothing.
    Chapters { chapters: Vec<(String, u64)> },
}

impl MediaJobOutput {
    /// The written file, when this job produced one.
    #[must_use]
    pub fn output(&self) -> Option<&Path> {
        match self {
            Self::File { output } | Self::FileWithChapters { output, .. } => Some(output),
            Self::Chapters { .. } => None,
        }
    }

    /// The chapter list, when this job produced one.
    #[must_use]
    pub fn chapters(&self) -> Option<&[(String, u64)]> {
        match self {
            Self::FileWithChapters { chapters, .. } | Self::Chapters { chapters } => Some(chapters),
            Self::File { .. } => None,
        }
    }
}

impl MediaJob {
    /// Paths the job reads but must not modify.
    ///
    /// These become the worker's read allowlist verbatim. Symbolic links are
    /// granted at their resolved target, because that is the inode the kernel
    /// checks — declaring a link inside the cache directory therefore grants
    /// whatever it points at. The host builds these paths from its own cache
    /// and output roots, so nothing untrusted chooses them.
    #[must_use]
    pub fn read_paths(&self) -> Vec<PathBuf> {
        match self {
            Self::EncodeMp3 { input, .. } | Self::RemuxTrimmed { input, .. } => {
                vec![input.clone()]
            }
            Self::Fixup { request } => {
                let mut paths = vec![request.input.clone()];
                paths.extend(request.cover.clone());
                paths
            }
            Self::PackageM4b { request } => request.parts.clone(),
            Self::AlignChapters { path, .. } => vec![path.clone()],
        }
    }

    /// Directories the job writes into.
    ///
    /// Directories rather than files, because the codecs stage output through
    /// temporary files alongside the destination and Landlock rules apply to a
    /// path and everything beneath it.
    #[must_use]
    pub fn write_dirs(&self) -> Vec<PathBuf> {
        let output = match self {
            Self::EncodeMp3 { output, .. } | Self::RemuxTrimmed { output, .. } => Some(output),
            Self::Fixup { request } => Some(&request.output),
            Self::PackageM4b { request } => Some(&request.output),
            Self::AlignChapters { .. } => None,
        };
        output
            .and_then(|path| path.parent())
            .map(|parent| vec![parent.to_path_buf()])
            .unwrap_or_default()
    }

    /// Short label for logs and error messages.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::EncodeMp3 { .. } => "encode_mp3",
            Self::RemuxTrimmed { .. } => "remux_trimmed",
            Self::Fixup { .. } => "fixup",
            Self::PackageM4b { .. } => "package_m4b",
            Self::AlignChapters { .. } => "align_chapters",
        }
    }

    /// Create the output directories this job needs.
    ///
    /// Both Landlock and Seatbelt reject a rule naming a path that does not
    /// exist, so the destination has to be created before the jail is built.
    ///
    /// # Errors
    ///
    /// Propagates directory-creation failures.
    pub fn prepare_output_dirs(&self) -> std::io::Result<()> {
        for dir in self.write_dirs() {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// Check that every declared input is readable.
    ///
    /// Called before the codecs run so a missing or jailed-away input produces
    /// [`crate::MediaError::InputMissing`] naming the path, rather than a bare
    /// `No such file or directory` from somewhere inside a decoder.
    fn check_inputs(&self) -> Result<()> {
        for path in self.read_paths() {
            if !path.exists() {
                return Err(crate::MediaError::InputMissing(path));
            }
        }
        Ok(())
    }

    /// Execute the job on the calling thread.
    ///
    /// This is the entry point the worker binary calls after confining itself.
    /// It is also used directly when process isolation is turned off.
    ///
    /// # Errors
    ///
    /// Returns [`crate::MediaError::InputMissing`] when a declared input cannot
    /// be reached, and propagates codec and I/O failures otherwise.
    pub fn run(self) -> Result<MediaJobOutput> {
        self.check_inputs()?;
        match self {
            Self::EncodeMp3 {
                input,
                output,
                lame,
                max_sample_rate,
            } => {
                crate::mp3::encode_to_mp3_native(&input, &output, &lame, max_sample_rate)?;
                Ok(MediaJobOutput::File { output })
            }
            Self::RemuxTrimmed {
                input,
                output,
                trim,
            } => {
                crate::native::remux_trimmed(&input, &output, trim)?;
                Ok(MediaJobOutput::File { output })
            }
            Self::Fixup { request } => {
                let output = request.output.clone();
                crate::metadata::fixup_audiobook_sync(&request)?;
                Ok(MediaJobOutput::File { output })
            }
            Self::PackageM4b { request } => {
                let (outcome, chapters) =
                    crate::package_m4b::package_m4b_from_parts_native(&request)?;
                Ok(MediaJobOutput::FileWithChapters {
                    output: outcome.output,
                    chapters,
                })
            }
            Self::AlignChapters {
                path,
                chapters,
                options,
            } => Ok(MediaJobOutput::Chapters {
                chapters: crate::chapter_align::align_chapter_starts(&path, &chapters, options),
            }),
        }
    }
}

/// Wire form of a finished job. `Result` is not `Serialize`, so the worker and
/// the pool exchange this instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MediaJobReply {
    /// The job succeeded.
    Ok(MediaJobOutput),
    /// The job failed. Carries the rendered error, since [`crate::MediaError`]
    /// is not reconstructible across a process boundary.
    Err { message: String },
}

impl From<Result<MediaJobOutput>> for MediaJobReply {
    fn from(result: Result<MediaJobOutput>) -> Self {
        match result {
            Ok(output) => Self::Ok(output),
            Err(err) => Self::Err {
                message: err.to_string(),
            },
        }
    }
}

/// Convenience for tests and for callers that only need the written file.
impl From<MediaJobOutput> for MediaOutcome {
    fn from(value: MediaJobOutput) -> Self {
        Self {
            output: value.output().map(Path::to_path_buf).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_job(input: &str, output: &str) -> MediaJob {
        MediaJob::EncodeMp3 {
            input: PathBuf::from(input),
            output: PathBuf::from(output),
            lame: Box::default(),
            max_sample_rate: None,
        }
    }

    #[test]
    fn encode_declares_its_input_and_output_directory() {
        let job = encode_job("/cache/in.m4b", "/out/book/ch1.mp3");
        assert_eq!(job.read_paths(), vec![PathBuf::from("/cache/in.m4b")]);
        assert_eq!(job.write_dirs(), vec![PathBuf::from("/out/book")]);
    }

    #[test]
    fn package_declares_every_part_as_readable() {
        let job = MediaJob::PackageM4b {
            request: Box::new(PackageM4bRequest {
                parts: vec![PathBuf::from("/cache/a.mp3"), PathBuf::from("/cache/b.mp3")],
                output: PathBuf::from("/out/book.m4b"),
                chapter_titles: vec![],
            }),
        };
        assert_eq!(job.read_paths().len(), 2);
        assert_eq!(job.write_dirs(), vec![PathBuf::from("/out")]);
    }

    #[test]
    fn align_writes_nothing() {
        let job = MediaJob::AlignChapters {
            path: PathBuf::from("/cache/book.m4b"),
            chapters: vec![("One".into(), 0)],
            options: ChapterAlignOptions::default(),
        };
        assert!(job.write_dirs().is_empty());
    }

    #[test]
    fn cover_art_is_declared_readable() {
        let mut request = fixup_request();
        request.cover = Some(PathBuf::from("/cache/cover.jpg"));
        let job = MediaJob::Fixup {
            request: Box::new(request),
        };
        assert!(job
            .read_paths()
            .contains(&PathBuf::from("/cache/cover.jpg")));
    }

    #[test]
    fn jobs_round_trip_through_json() {
        let job = encode_job("/cache/in.m4b", "/out/ch1.mp3");
        let encoded = serde_json::to_string(&job).expect("serialize");
        let decoded: MediaJob = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded.label(), "encode_mp3");
        assert_eq!(decoded.read_paths(), job.read_paths());
    }

    #[test]
    fn replies_round_trip_through_json() {
        let reply = MediaJobReply::Ok(MediaJobOutput::FileWithChapters {
            output: PathBuf::from("/out/book.m4b"),
            chapters: vec![("One".into(), 0), ("Two".into(), 1_000)],
        });
        let encoded = serde_json::to_string(&reply).expect("serialize");
        let decoded: MediaJobReply = serde_json::from_str(&encoded).expect("deserialize");
        match decoded {
            MediaJobReply::Ok(output) => {
                assert_eq!(output.chapters().map(<[_]>::len), Some(2));
                assert_eq!(output.output(), Some(Path::new("/out/book.m4b")));
            }
            MediaJobReply::Err { message } => panic!("expected ok, got {message}"),
        }
    }

    fn fixup_request() -> FixupRequest {
        FixupRequest {
            input: PathBuf::from("/cache/in.m4b"),
            output: PathBuf::from("/out/book.m4b"),
            title: "Title".into(),
            author: None,
            narrator: None,
            cover: None,
            chapters: vec![],
            replace_chapters: false,
            subtitle: None,
            publisher: None,
            year: None,
            genre: None,
            series: None,
            series_index: None,
            asin: None,
            isbn: None,
            description: None,
            language: None,
            tool: None,
        }
    }
}
