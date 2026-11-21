//! Expanding uploaded archives in the confined worker, and checking what the
//! worker says it wrote.
//!
//! The worker walks the archive with `bookclerk-archive` into an empty folder
//! the host created, under caps the host chose. Its report is untrusted (the
//! worker just parsed hostile bytes), so [`extract_archive`] validates it and
//! checks every listed file on disk before handing it back.

use std::path::Path;

use bookclerk_archive::{DirSink, ExtractReport, Limits};

use crate::error::{MediaError, Result};
use crate::job::{MediaJob, MediaJobOutput};
use crate::pool::pool;

/// Runs the walk inside the worker, after it has confined itself.
///
/// # Errors
///
/// [`MediaError::Archive`] when the input is not an archive this worker
/// expands, and [`MediaError::Io`] when it cannot be read.
pub(crate) fn extract_in_worker(
    input: &Path,
    output_dir: &Path,
    limits: &Limits,
) -> Result<ExtractReport> {
    let file = std::fs::File::open(input)?;
    let mut sink = DirSink::new(output_dir);
    bookclerk_archive::extract(std::io::BufReader::new(file), limits, &mut sink).map_err(|err| {
        match err {
            bookclerk_archive::ExtractError::Io(io) => MediaError::Io(io),
            bookclerk_archive::ExtractError::NotArchive => MediaError::Archive(err.to_string()),
        }
    })
}

/// Expands `input` into the empty folder `output_dir` in a confined media
/// worker, then checks the result.
///
/// The report is validated against `limits` ([`ExtractReport::validate`]),
/// and every file it lists must be a regular file (not a link) at its path
/// under `output_dir`, with the size the report states. A stopped walk is
/// returned as is: the caller discards the folder.
///
/// # Arguments
///
/// * `input` - The archive.
/// * `output_dir` - An empty folder; created if missing.
/// * `limits` - The caps for this archive.
///
/// # Errors
///
/// [`MediaError::Archive`] when `output_dir` is not empty, the input is not
/// a recognised archive, or the report does not hold up;
/// [`MediaError::NotIsolated`] when isolation is required and no worker is
/// available; worker and I/O failures otherwise.
pub async fn extract_archive(
    input: &Path,
    output_dir: &Path,
    limits: Limits,
) -> Result<ExtractReport> {
    if !input.exists() {
        return Err(MediaError::InputMissing(input.to_path_buf()));
    }
    std::fs::create_dir_all(output_dir)?;
    if std::fs::read_dir(output_dir)?.next().is_some() {
        return Err(MediaError::Archive(format!(
            "output folder {} is not empty",
            output_dir.display()
        )));
    }
    let output = pool()
        .run(MediaJob::ExtractArchive {
            input: input.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
            limits,
        })
        .await?;
    let MediaJobOutput::Extracted { report } = output else {
        return Err(MediaError::Archive(
            "the worker answered with another job's result".into(),
        ));
    };
    check_report(&report, output_dir, &limits)?;
    Ok(*report)
}

/// Checks a worker's report against the limits and the files on disk.
///
/// # Errors
///
/// [`MediaError::Archive`] naming what does not hold.
pub fn check_report(report: &ExtractReport, output_dir: &Path, limits: &Limits) -> Result<()> {
    report
        .validate(limits)
        .map_err(|err| MediaError::Archive(err.to_string()))?;
    for entry in &report.written {
        let path = entry
            .path
            .components()
            .fold(output_dir.to_path_buf(), |dir, part| dir.join(part));
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|err| MediaError::Archive(format!("{}: {err}", entry.path)))?;
        if !meta.file_type().is_file() || meta.len() != entry.bytes {
            return Err(MediaError::Archive(format!(
                "{} is not the file the report describes",
                entry.path
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_archive::fixtures::{zip_of, ZipEntry};

    #[test]
    fn the_in_worker_walk_writes_and_the_check_accepts_it() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.zip");
        std::fs::write(
            &input,
            zip_of(&[
                ZipEntry::Stored("Book/01.mp3", b"audio"),
                ZipEntry::Symlink("Book/link", "/etc/passwd"),
            ]),
        )
        .unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let limits = Limits::UPLOAD_DEFAULTS;
        let report = extract_in_worker(&input, &out, &limits).unwrap();
        assert_eq!(report.written.len(), 1);
        assert_eq!(report.refused.len(), 1);
        check_report(&report, &out, &limits).unwrap();
    }

    #[test]
    fn the_check_refuses_a_report_that_lies_about_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = ExtractReport::new(bookclerk_archive::ArchiveFormat::Zip);
        report.written.push(bookclerk_archive::WrittenEntry {
            path: bookclerk_archive::EntryPath::admit("missing.mp3").unwrap(),
            bytes: 3,
        });
        report.total_bytes = 3;
        let limits = Limits::UPLOAD_DEFAULTS;
        assert!(check_report(&report, dir.path(), &limits).is_err());
        std::fs::write(dir.path().join("missing.mp3"), b"four").unwrap();
        assert!(check_report(&report, dir.path(), &limits).is_err());
        std::fs::write(dir.path().join("missing.mp3"), b"abc").unwrap();
        check_report(&report, dir.path(), &limits).unwrap();
        #[cfg(unix)]
        {
            std::fs::remove_file(dir.path().join("missing.mp3")).unwrap();
            std::fs::write(dir.path().join("real"), b"abc").unwrap();
            std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("missing.mp3"))
                .unwrap();
            assert!(check_report(&report, dir.path(), &limits).is_err());
        }
    }

    #[test]
    fn a_non_empty_output_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.zip");
        std::fs::write(&input, zip_of(&[ZipEntry::Stored("a", b"a")])).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("leftover"), b"x").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = runtime
            .block_on(extract_archive(&input, &out, Limits::UPLOAD_DEFAULTS))
            .unwrap_err();
        assert!(matches!(err, MediaError::Archive(_)), "{err}");
    }
}
