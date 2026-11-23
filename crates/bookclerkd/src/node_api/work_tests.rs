//! `deployments.list` paging: every deployment is listed exactly once
//! across pages that each fit a frame, and an entry too large for any frame
//! is left out without stalling the cursor.

use bookclerk_node_proto::{
    Body, RemoteArtifact, RemoteDeployment, RemoteVersion, MAX_FRAME_BYTES,
};

use super::deployments_page;

/// A deployment whose manifest is `manifest_bytes` long.
fn deployment(n: usize, manifest_bytes: usize) -> RemoteDeployment {
    RemoteDeployment {
        deployment_id: format!("dep-{n:04}"),
        instance_id: format!("inst-{n:04}"),
        plugin_key: format!("path:file:///plugins/p{n}#p{n}"),
        desired: "present".into(),
        revision: 1,
        default_instance: true,
        version: Some(RemoteVersion {
            version: "1.0.0".into(),
            manifest_id: format!("p{n}"),
            manifest_toml: "#".repeat(manifest_bytes),
            settings: serde_json::json!({}),
            artifacts: vec![RemoteArtifact {
                target: "any".into(),
                url: "blob:sha256:00".into(),
                format: "tar.gz".into(),
                archive_root: String::new(),
                archive_sha256: "00".into(),
                payload_root_sha256: "00".into(),
                executable: "run".into(),
            }],
            grant_json: String::new(),
            publisher_verified: false,
        }),
    }
}

/// Pages through `items` as a branch does; answers each page's ids and
/// every oversized id reported.
fn page_through(items: &[RemoteDeployment]) -> (Vec<Vec<String>>, Vec<String>) {
    let mut pages = Vec::new();
    let mut oversized = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = deployments_page("q1", items.to_vec(), after.as_deref());
        assert!(page.frame.to_text().len() <= MAX_FRAME_BYTES);
        oversized.extend(page.oversized);
        let Body::Deployments { items, next } = page.frame.body else {
            panic!("not a deployments page: {:?}", page.frame.body);
        };
        pages.push(items.into_iter().map(|d| d.deployment_id).collect());
        match next {
            Some(next) => {
                assert!(
                    after.as_ref().is_none_or(|after| next > *after),
                    "the cursor moves"
                );
                after = Some(next);
            }
            None => return (pages, oversized),
        }
    }
}

#[test]
fn a_small_list_is_one_page() {
    let items: Vec<_> = (0..3).rev().map(|n| deployment(n, 100)).collect();
    let (pages, oversized) = page_through(&items);
    assert_eq!(pages, [["dep-0000", "dep-0001", "dep-0002"]]);
    assert!(oversized.is_empty());
}

#[test]
fn a_list_larger_than_a_frame_is_paged_without_gaps_or_repeats() {
    // Each entry is ~200 KiB: five fit a frame, twelve need three pages.
    let items: Vec<_> = (0..12).map(|n| deployment(n, 200 * 1024)).collect();
    let (pages, oversized) = page_through(&items);
    assert!(pages.len() >= 3, "{} pages", pages.len());
    assert!(pages.iter().all(|page| !page.is_empty()));
    let listed: Vec<String> = pages.concat();
    let expected: Vec<String> = items.iter().map(|d| d.deployment_id.clone()).collect();
    assert_eq!(listed, expected);
    assert!(oversized.is_empty());
}

#[test]
fn an_entry_too_large_for_any_frame_is_left_out_and_passed() {
    let items = vec![
        deployment(0, 100),
        deployment(1, MAX_FRAME_BYTES),
        deployment(2, 100),
    ];
    let (pages, oversized) = page_through(&items);
    assert_eq!(pages.concat(), ["dep-0000", "dep-0002"]);
    assert_eq!(oversized, ["dep-0001"]);
}

#[test]
fn a_cursor_past_the_end_answers_an_empty_last_page() {
    let items: Vec<_> = (0..3).map(|n| deployment(n, 100)).collect();
    let page = deployments_page("q1", items, Some("zzz"));
    assert!(matches!(
        page.frame.body,
        Body::Deployments { ref items, next: None } if items.is_empty()
    ));
}
