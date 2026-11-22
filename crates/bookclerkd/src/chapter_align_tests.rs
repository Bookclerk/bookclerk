//! Tests for the `chapter_align` job's copy choice and scratch naming.

use bookclerk_library::{AudioStream, FileRole, ProbedAudioFile};

use super::*;

/// A probed audio file on `destination` at `key` with `role`, `ms` long.
fn file(destination: &str, key: &str, role: FileRole, ms: Option<u64>) -> ProbedAudioFile {
    ProbedAudioFile {
        destination: destination.into(),
        storage_key: key.into(),
        role,
        copy: None,
        probe: None,
        stream: AudioStream {
            duration_ms: ms,
            ..AudioStream::default()
        },
    }
}

/// A stand-in destination named `id`.
fn destination(id: &str, root: &Path) -> OwnerDestination {
    OwnerDestination {
        id: id.into(),
        backend: Box::new(
            bookclerk_storage::LocalFsBackend::new(root.to_path_buf())
                .expect("a local destination"),
        ),
        options: bookclerk_source::DownloadOptions::default(),
    }
}

#[test]
fn the_copy_is_the_first_timed_one_here_audio_then_parts_in_key_order() {
    let root = tempfile::tempdir().unwrap();
    let destinations = [destination("a", root.path()), destination("b", root.path())];
    let files = [
        // On `a` one part was never probed, so `a` has no timed copy.
        file("a", "x/1.mp3", FileRole::Part, Some(10)),
        file("a", "x/2.mp3", FileRole::Part, None),
        file("b", "y/2.mp3", FileRole::Part, Some(10)),
        file("b", "y/1.mp3", FileRole::Part, Some(10)),
        file("b", "y/c1.m4b", FileRole::Chapter, Some(5)),
    ];
    let (chosen, copy) = copy_here(&destinations, &files).expect("a copy");
    assert_eq!(chosen.id, "b");
    let keys: Vec<&str> = copy.iter().map(|f| f.storage_key.as_str()).collect();
    assert_eq!(keys, ["y/1.mp3", "y/2.mp3"]);

    let chapters_only = [file("a", "z/c1.m4b", FileRole::Chapter, Some(5))];
    let (_, copy) = copy_here(&destinations, &chapters_only).expect("chapter files");
    assert_eq!(copy.len(), 1);
    assert!(copy_here(&destinations, &[]).is_none());
}

#[test]
fn an_extension_is_kept_only_when_it_is_short_and_plain() {
    assert_eq!(extension("Book/Book.M4B"), "m4b");
    assert_eq!(extension("Book/noext"), "bin");
    assert_eq!(extension("Book/odd.e x"), "bin");
    assert_eq!(extension("a.b/c"), "bin");
}
