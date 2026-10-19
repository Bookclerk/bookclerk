//! Replays the checked-in `mp4_parse` fuzz corpus in PR CI (no libFuzzer
//! required), so an input that once crashed the parser stays fixed.

use bookclerk_mp4::{parse_mp4, remux_progressive, CopySamples, RemuxOptions};

#[test]
fn fuzz_corpus_mp4_parse_does_not_panic() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/mp4_parse")
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    let out_dir = tempfile::tempdir().expect("tempdir");
    let output = out_dir.path().join("out.m4b");
    let mut replayed = 0usize;
    for entry in std::fs::read_dir(&dir).expect("fuzz corpus") {
        let entry = entry.expect("entry");
        // Inline canonicalize → starts_with at each sink (CodeQL two-state barrier).
        let Ok(path) = entry.path().canonicalize() else {
            continue;
        };
        if !path.starts_with(&dir) || !path.is_file() {
            continue;
        }
        if let Ok(mp4) = parse_mp4(&path) {
            let _ = bookclerk_mp4::track_duration_ms(&mp4.audio);
            let _ = bookclerk_mp4::extract_mp4a_config(&mp4);
        }
        let _ = remux_progressive(&path, &output, &RemuxOptions::default(), &mut CopySamples);
        replayed += 1;
    }
    assert!(replayed > 0, "no corpus files under {}", dir.display());
}
