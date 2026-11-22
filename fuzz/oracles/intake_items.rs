//! Shared oracle for the `intake_items` fuzz target and the
//! `bookclerk-intake` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A drop folder's listing reaches `bookclerk_intake::ItemGrouper` from a
//! folder anyone with access to it can fill. The input's first byte picks
//! the attribution mode (low bit) and a quiet period; the rest is lines of
//! `key<TAB>size<TAB>signature`, one listed object per line. The
//! invariants, checked against an independent reading:
//!
//! - every object lands where the oracle puts it: ignored when a segment is
//!   empty, `.` or `..`, starts with `.`, holds a control character, names
//!   `_imported` or `_failed` at the root, or matches an ignore pattern, or
//!   when the key is empty, over 1,024 bytes or holds a backslash; a stray
//!   when it is shallower than the mode's item depth; otherwise a member of
//!   the item named by its first one or two segments;
//! - the items are exactly the oracle's, with the same members (last
//!   listing of a repeated key wins), saturating byte totals, and the
//!   member cap flagged exactly when a distinct member past it was listed;
//! - the signature does not depend on listing order, and changes when any
//!   member's size or signature changes or the item goes past the cap;
//!
//! Ignore patterns are matched here by a table-driven reference matcher
//! written from the documented rules (`*` any run, `?` one character, ASCII
//! case folded), not by the production `glob_matches`.
//! - `step` makes an item ready only once the quiet period has passed with
//!   its signature unchanged, never for an item over the member cap, and
//!   never forgets a queued item.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_config::{IntakeAttribution, IntakeSettings, BUILTIN_IGNORE_PATTERNS};
use bookclerk_intake::{
    signature_of, step, ItemGrouper, ItemLimit, ListedObject, Placement, Step, TrackedItem,
    TrackedState, MAX_ITEM_MEMBERS,
};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// One listed object read from a line.
struct Line<'a> {
    /// Object key.
    key: &'a str,
    /// Size in bytes.
    size: u64,
    /// Change signature.
    signature: &'a str,
}

/// Reads the input's lines; a line that is not `key\tsize\tsig` is skipped.
fn lines(text: &str) -> Vec<Line<'_>> {
    text.split('\n')
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let key = parts.next()?;
            let size = parts.next()?.parse().ok()?;
            let signature = parts.next()?;
            Some(Line {
                key,
                size,
                signature,
            })
        })
        .collect()
}

/// The reference glob matcher: `matches[i][j]` says whether the first `i`
/// pattern characters match the first `j` name characters, where `*`
/// matches any run (empty included), `?` one character, and anything else
/// itself without ASCII case.
fn reference_glob(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut matches = vec![vec![false; name.len() + 1]; pattern.len() + 1];
    matches[0][0] = true;
    for i in 1..=pattern.len() {
        let p = pattern[i - 1];
        for j in 0..=name.len() {
            matches[i][j] = match p {
                '*' => matches[i - 1][j] || (j > 0 && matches[i][j - 1]),
                '?' => j > 0 && matches[i - 1][j - 1],
                c => j > 0 && matches[i - 1][j - 1] && c.eq_ignore_ascii_case(&name[j - 1]),
            };
        }
    }
    matches[pattern.len()][name.len()]
}

/// The oracle's placement of `key`: `None` ignored, `Some(None)` stray,
/// `Some(Some(item))` a member of `item`.
fn oracle_place(settings: &IntakeSettings, key: &str) -> Option<Option<String>> {
    if key.is_empty() || key.len() > 1024 || key.contains('\\') {
        return None;
    }
    let segments: Vec<&str> = key.split('/').collect();
    for (i, segment) in segments.iter().enumerate() {
        let host_folder = i == 0 && (*segment == "_imported" || *segment == "_failed");
        let patterned = BUILTIN_IGNORE_PATTERNS
            .iter()
            .copied()
            .chain(settings.ignore.iter().map(String::as_str))
            .any(|pattern| reference_glob(pattern, segment));
        if segment.is_empty()
            || segment.starts_with('.')
            || segment.chars().any(char::is_control)
            || host_folder
            || patterned
        {
            return None;
        }
    }
    let depth = match settings.attribute_to {
        IntakeAttribution::Owner => 1,
        IntakeAttribution::Subfolder => 2,
    };
    if segments.len() < depth {
        return Some(None);
    }
    Some(Some(segments[..depth].join("/")))
}

/// Checks the grouper and `step` against the oracle on `data`.
///
/// # Panics
///
/// Panics when an invariant fails.
// Oracle code: a disagreement is the finding, reported by panicking.
#[allow(clippy::expect_used, clippy::panic)]
pub fn check(data: &[u8]) {
    if data.is_empty() || data.len() > MAX_INPUT_BYTES {
        return;
    }
    let mode = data[0];
    let Ok(text) = std::str::from_utf8(&data[1..]) else {
        return;
    };
    let settings = IntakeSettings {
        attribute_to: if mode & 1 == 0 {
            IntakeAttribution::Owner
        } else {
            IntakeAttribution::Subfolder
        },
        patron: (mode & 1 == 0).then_some(1),
        ignore: vec!["*.nzb".to_string(), "sample?".to_string()],
        ..IntakeSettings::for_patron(1)
    };
    let settle_ms = u64::from(mode >> 1) * 1_000;
    let listed = lines(text);

    // Placement and grouping.
    let mut grouper = ItemGrouper::new(&settings);
    let mut expected: BTreeMap<String, BTreeMap<String, (u64, String)>> = BTreeMap::new();
    let mut over_cap: BTreeSet<String> = BTreeSet::new();
    for line in &listed {
        let placement = grouper.add(ListedObject {
            key: line.key,
            size: line.size,
            signature: line.signature,
        });
        let oracle = oracle_place(&settings, line.key);
        match (&placement, &oracle) {
            (Placement::Ignored, None) | (Placement::Stray, Some(None)) => {}
            (Placement::Member(item), Some(Some(want))) => {
                assert_eq!(item, want, "key {:?}", line.key);
                let members = expected.entry(want.clone()).or_default();
                if members.contains_key(line.key) || members.len() < MAX_ITEM_MEMBERS {
                    members.insert(
                        line.key.to_string(),
                        (line.size, line.signature.to_string()),
                    );
                } else {
                    over_cap.insert(want.clone());
                }
            }
            _ => panic!("key {:?} placed {placement:?}, oracle {oracle:?}", line.key),
        }
    }
    let items = grouper.finish();
    assert_eq!(
        items.iter().map(|i| i.key.clone()).collect::<Vec<_>>(),
        expected.keys().cloned().collect::<Vec<_>>()
    );
    for item in &items {
        let want = &expected[&item.key];
        let got: BTreeMap<String, (u64, String)> = item
            .members
            .iter()
            .map(|m| (m.key.clone(), (m.size, m.signature.clone())))
            .collect();
        assert_eq!(&got, want, "members of {:?}", item.key);
        let total = want
            .values()
            .fold(0u64, |sum, (size, _)| sum.saturating_add(*size));
        assert_eq!(item.total_bytes, total);
        let capped = over_cap.contains(&item.key);
        assert_eq!(
            item.limit,
            capped.then_some(ItemLimit::TooManyMembers),
            "member cap of {:?}",
            item.key
        );
        assert_eq!(
            item.members.len() == MAX_ITEM_MEMBERS,
            want.len() == MAX_ITEM_MEMBERS
        );
        if capped {
            assert_ne!(
                item.signature,
                signature_of(&item.members, None),
                "going past the cap changes the signature of {:?}",
                item.key
            );
        }
        match settings.attribute_to {
            IntakeAttribution::Owner => assert_eq!(item.patron_folder, None),
            IntakeAttribution::Subfolder => {
                assert_eq!(item.patron_folder.as_deref(), item.key.split('/').next())
            }
        }
    }

    // Order independence: the reversed listing gives the same signatures
    // (when no key repeats, since the last listing of a key wins).
    let mut seen = BTreeSet::new();
    if listed.iter().all(|line| seen.insert(line.key)) {
        let mut reversed = ItemGrouper::new(&settings);
        for line in listed.iter().rev() {
            reversed.add(ListedObject {
                key: line.key,
                size: line.size,
                signature: line.signature,
            });
        }
        let again = reversed.finish();
        let capped = items.iter().any(|item| item.limit.is_some());
        if !capped {
            assert_eq!(
                items.iter().map(|i| i.signature).collect::<Vec<_>>(),
                again.iter().map(|i| i.signature).collect::<Vec<_>>()
            );
        }
    }

    // A changed size changes the signature.
    if let Some(item) = items.first() {
        if let Some(first) = item.members.first() {
            let mut bumped = ItemGrouper::new(&settings);
            for member in &item.members {
                bumped.add(ListedObject {
                    key: &member.key,
                    size: if member.key == first.key {
                        member.size.wrapping_add(1)
                    } else {
                        member.size
                    },
                    signature: &member.signature,
                });
            }
            assert_ne!(bumped.finish()[0].signature, item.signature);
        }
    }

    // The quiet period.
    for item in &items {
        let Step::Store(record) = step(None, Some(item), 1_000, settle_ms) else {
            panic!("a new item is stored");
        };
        assert_eq!(record.state, TrackedState::Settling);
        if settle_ms > 0 {
            assert_eq!(
                step(Some(&record), Some(item), 1_000 + settle_ms - 1, settle_ms),
                Step::Keep
            );
        }
        let later = step(Some(&record), Some(item), 1_000 + settle_ms, settle_ms);
        match item.limit {
            Some(_) => assert_eq!(later, Step::Keep),
            None => assert_eq!(
                later,
                Step::Ready(TrackedItem {
                    state: TrackedState::Queued,
                    ..record
                })
            ),
        }
        let queued = TrackedItem {
            state: TrackedState::Queued,
            ..record
        };
        assert_eq!(step(Some(&queued), None, u64::MAX, settle_ms), Step::Keep);
    }
}
