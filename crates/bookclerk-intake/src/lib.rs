//! Drop folder items and when they have settled, without IO.
//!
//! A drop folder is a storage plugin instance with the intake role (see
//! `docs/server/drop-folders.md`). The host lists it page by page and feeds
//! every listed object to an [`ItemGrouper`], which:
//!
//! 1. drops objects that are never part of an item: names starting with
//!    `.`, partial downloads and temporaries, the host's own `_imported` and
//!    `_failed` folders, and keys that are not a clean relative path;
//! 2. groups the rest into items, one per top-level entry (one level
//!    deeper in `subfolder` mode, where the top level names the patron);
//! 3. gives each item a [`ItemSignature`] over its members' keys, sizes and
//!    change signatures, independent of listing order.
//!
//! [`step`] then compares each item with what the host stored for it last
//! time and says what to store now, and whether the item is ready to
//! ingest: an item is ready once its signature has stayed the same for the
//! folder's quiet period. No portable way exists to see whether another
//! program still has a file open, so an unchanged signature over time is
//! the only test of a finished copy.
//!
//! Keys come from the folder, which anyone with access to it can fill, so
//! this crate is an untrusted-input boundary: the `intake_items` fuzz
//! target checks it against an independent reading.

use std::collections::BTreeMap;

use bookclerk_config::IntakeSettings;
use sha2::{Digest, Sha256};

/// Longest object key the grouper accepts, in UTF-8 bytes (the largest
/// object-store key).
pub const MAX_KEY_BYTES: usize = 1024;

/// Most member files one item may hold. An item with more is reported as
/// [`ItemLimit::TooManyMembers`] and never becomes ready.
pub const MAX_ITEM_MEMBERS: usize = 4096;

/// One object as a drop folder listing reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedObject<'a> {
    /// Object key relative to the drop folder's root, `/`-separated.
    pub key: &'a str,
    /// Size in bytes.
    pub size: u64,
    /// The backend's change signature (`ObjectInfo.signature`, else its
    /// ETag or modification time); empty when the backend has none, in
    /// which case only the size can show a change.
    pub signature: &'a str,
}

/// Where the grouper put one listed object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// A member of the item with this key.
    Member(String),
    /// Never part of an item: an ignored name, a host folder, or a key
    /// that is not a clean relative path.
    Ignored,
    /// A file directly under the root in `subfolder` mode, which belongs
    /// to no patron folder; it is left alone.
    Stray,
}

/// A member file of an item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemMember {
    /// Object key relative to the drop folder's root.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// The backend's change signature, possibly empty.
    pub signature: String,
}

/// Why an item can never become ready as listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemLimit {
    /// The item holds more than [`MAX_ITEM_MEMBERS`] files.
    TooManyMembers,
}

/// A digest of an item's members, independent of listing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemSignature([u8; 32]);

impl ItemSignature {
    /// The signature as 64 lowercase hex characters, as the host stores it.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    /// Reads a stored signature; `None` for anything but 64 hex characters.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(text, &mut bytes).ok()?;
        Some(Self(bytes))
    }
}

/// One item as a whole listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedItem {
    /// The item's key: its top-level name, or `<patron folder>/<name>` in
    /// `subfolder` mode.
    pub key: String,
    /// The patron folder's name in `subfolder` mode; `None` in `owner`
    /// mode.
    pub patron_folder: Option<String>,
    /// Member files sorted by key, at most [`MAX_ITEM_MEMBERS`].
    pub members: Vec<ItemMember>,
    /// Sum of the members' sizes, saturating.
    pub total_bytes: u64,
    /// Digest of every member's key, size and signature, and of whether
    /// the item went past [`MAX_ITEM_MEMBERS`] (so a member added past the
    /// cap still changes it).
    pub signature: ItemSignature,
    /// Set when the item can never become ready as listed.
    pub limit: Option<ItemLimit>,
}

/// Groups a drop folder's listing into items.
#[derive(Debug)]
pub struct ItemGrouper<'s> {
    /// The folder's settings: ignore patterns and attribution mode.
    settings: &'s IntakeSettings,
    /// Members seen so far: size and signature by member key, by item key.
    items: BTreeMap<String, BTreeMap<String, (u64, String)>>,
    /// Items that went past [`MAX_ITEM_MEMBERS`].
    over_limit: BTreeMap<String, ()>,
}

impl<'s> ItemGrouper<'s> {
    /// An empty grouper for one listing of a folder with `settings`.
    #[must_use]
    pub fn new(settings: &'s IntakeSettings) -> Self {
        Self {
            settings,
            items: BTreeMap::new(),
            over_limit: BTreeMap::new(),
        }
    }

    /// Adds one listed object and says where it went.
    ///
    /// A key listed twice counts once, with the size and signature of the
    /// last listing.
    pub fn add(&mut self, object: ListedObject<'_>) -> Placement {
        let placement = place(self.settings, object.key);
        if let Placement::Member(item) = &placement {
            let members = self.items.entry(item.clone()).or_default();
            let entry = (object.size, object.signature.to_string());
            if let Some(existing) = members.get_mut(object.key) {
                *existing = entry;
            } else if members.len() < MAX_ITEM_MEMBERS {
                members.insert(object.key.to_string(), entry);
            } else {
                self.over_limit.insert(item.clone(), ());
            }
        }
        placement
    }

    /// Every item seen, sorted by key.
    #[must_use]
    pub fn finish(self) -> Vec<ObservedItem> {
        let attribution = self.settings.attribute_to;
        self.items
            .into_iter()
            .map(|(key, members)| {
                let members: Vec<ItemMember> = members
                    .into_iter()
                    .map(|(key, (size, signature))| ItemMember {
                        key,
                        size,
                        signature,
                    })
                    .collect();
                let total_bytes = members
                    .iter()
                    .fold(0u64, |sum, member| sum.saturating_add(member.size));
                let patron_folder = match attribution {
                    bookclerk_config::IntakeAttribution::Owner => None,
                    bookclerk_config::IntakeAttribution::Subfolder => {
                        key.split('/').next().map(str::to_string)
                    }
                };
                let limit = self
                    .over_limit
                    .contains_key(&key)
                    .then_some(ItemLimit::TooManyMembers);
                ObservedItem {
                    signature: signature_of(&members, limit),
                    key,
                    patron_folder,
                    members,
                    total_bytes,
                    limit,
                }
            })
            .collect()
    }
}

/// Where `key` belongs under `settings`.
fn place(settings: &IntakeSettings, key: &str) -> Placement {
    if key.is_empty() || key.len() > MAX_KEY_BYTES || key.contains('\\') {
        return Placement::Ignored;
    }
    let segments: Vec<&str> = key.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || segment.chars().any(char::is_control)
            || settings.ignores(segment, index == 0)
        {
            return Placement::Ignored;
        }
    }
    let depth = settings.attribute_to.item_depth();
    if segments.len() < depth {
        return Placement::Stray;
    }
    Placement::Member(segments[..depth].join("/"))
}

/// The order-independent digest of sorted `members`: each member's key,
/// size and signature, length-prefixed so no two lists collide by
/// concatenation, then a marker when the item is over a limit. An item
/// within every limit hashes exactly as before the marker existed.
///
/// # Arguments
///
/// * `members` - The members, sorted by key.
/// * `limit` - The limit the item went past, if any.
#[must_use]
pub fn signature_of(members: &[ItemMember], limit: Option<ItemLimit>) -> ItemSignature {
    let mut hasher = Sha256::new();
    hasher.update(b"bookclerk-intake-item-v1");
    for member in members {
        for part in [member.key.as_bytes(), member.signature.as_bytes()] {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part);
        }
        hasher.update(member.size.to_be_bytes());
    }
    match limit {
        None => {}
        // Read as the start of another member, the trailer's first eight
        // bytes are a length far past its end, so no member list hashes
        // the same bytes.
        Some(ItemLimit::TooManyMembers) => hasher.update(b"\xFFover-limit:members"),
    }
    ItemSignature(hasher.finalize().into())
}

/// What the host stored about one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedState {
    /// Seen, not yet quiet for long enough.
    Settling,
    /// Handed to an ingest job that has not finished.
    Queued,
    /// Ingested and left in place (`after_ingest = leave`); ingested again
    /// only if it changes.
    Done,
    /// Ingest failed; tried again only if it changes.
    Failed,
}

/// One item's stored record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackedItem {
    /// The item's state.
    pub state: TrackedState,
    /// The signature the state applies to.
    pub signature: ItemSignature,
    /// When the signature was first seen unchanged, in unix milliseconds.
    pub quiet_since_ms: u64,
}

/// What to do with one item's record after a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing changed.
    Keep,
    /// Store this record (new or changed).
    Store(TrackedItem),
    /// The item has settled: store this `Queued` record and queue one
    /// ingest job for it.
    Ready(TrackedItem),
    /// The item is gone: delete its record.
    Forget,
}

/// Compares one item's stored record with the latest full listing.
///
/// # Arguments
///
/// * `tracked` - The stored record, if any.
/// * `observed` - The item as the listing shows it; `None` when the
///   listing no longer has it.
/// * `now_ms` - The current time, unix milliseconds.
/// * `settle_ms` - The folder's quiet period, milliseconds.
///
/// # Returns
///
/// What to store. A queued item keeps its record until the ingest job
/// finishes, even if its files changed or vanished: the job compares the
/// signature before it reads and reports the item failed if it moved.
#[must_use]
pub fn step(
    tracked: Option<&TrackedItem>,
    observed: Option<&ObservedItem>,
    now_ms: u64,
    settle_ms: u64,
) -> Step {
    let settling = |signature| TrackedItem {
        state: TrackedState::Settling,
        signature,
        quiet_since_ms: now_ms,
    };
    match (tracked, observed) {
        (None, None) => Step::Keep,
        (None, Some(item)) => Step::Store(settling(item.signature)),
        (Some(record), None) => match record.state {
            TrackedState::Queued => Step::Keep,
            TrackedState::Settling | TrackedState::Done | TrackedState::Failed => Step::Forget,
        },
        (Some(record), Some(item)) => {
            if record.signature != item.signature {
                return match record.state {
                    TrackedState::Queued => Step::Keep,
                    TrackedState::Settling | TrackedState::Done | TrackedState::Failed => {
                        Step::Store(settling(item.signature))
                    }
                };
            }
            match record.state {
                TrackedState::Queued | TrackedState::Done | TrackedState::Failed => Step::Keep,
                TrackedState::Settling => {
                    let quiet = now_ms.saturating_sub(record.quiet_since_ms) >= settle_ms;
                    if quiet && item.limit.is_none() {
                        Step::Ready(TrackedItem {
                            state: TrackedState::Queued,
                            ..*record
                        })
                    } else {
                        Step::Keep
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_config::{IntakeAttribution, IntakeSettings};

    fn obj<'a>(key: &'a str, size: u64, sig: &'a str) -> ListedObject<'a> {
        ListedObject {
            key,
            size,
            signature: sig,
        }
    }

    fn group(settings: &IntakeSettings, objects: &[ListedObject<'_>]) -> Vec<ObservedItem> {
        let mut grouper = ItemGrouper::new(settings);
        for object in objects {
            grouper.add(*object);
        }
        grouper.finish()
    }

    #[test]
    fn top_level_entries_are_items() {
        let settings = IntakeSettings::for_patron(1);
        let items = group(
            &settings,
            &[
                obj("Book A/01.mp3", 10, "s1"),
                obj("Book A/02.mp3", 20, "s2"),
                obj("Book A/.DS_Store", 1, "x"),
                obj("Book B.m4b", 30, "s3"),
                obj("Book C.m4b.part", 5, "p"),
                obj("_imported/2026-11-20/Old.m4b", 9, "o"),
                obj("../escape.m4b", 9, "e"),
                obj("a//b.m4b", 9, "e"),
            ],
        );
        let keys: Vec<&str> = items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(keys, ["Book A", "Book B.m4b"]);
        assert_eq!(items[0].members.len(), 2);
        assert_eq!(items[0].total_bytes, 30);
        assert_eq!(items[0].patron_folder, None);
    }

    #[test]
    fn subfolder_mode_names_the_patron_and_leaves_strays() {
        let settings = IntakeSettings {
            attribute_to: IntakeAttribution::Subfolder,
            patron: None,
            ..IntakeSettings::for_patron(1)
        };
        let mut grouper = ItemGrouper::new(&settings);
        assert_eq!(grouper.add(obj("loose.m4b", 1, "")), Placement::Stray);
        assert_eq!(
            grouper.add(obj("alex/Book/01.mp3", 1, "")),
            Placement::Member("alex/Book".to_string())
        );
        let items = grouper.finish();
        assert_eq!(items[0].patron_folder.as_deref(), Some("alex"));
    }

    #[test]
    fn signature_ignores_order_and_sees_every_change() {
        let settings = IntakeSettings::for_patron(1);
        let a = group(&settings, &[obj("B/1", 1, "x"), obj("B/2", 2, "y")]);
        let b = group(&settings, &[obj("B/2", 2, "y"), obj("B/1", 1, "x")]);
        assert_eq!(a[0].signature, b[0].signature);
        for changed in [
            [obj("B/1", 1, "x"), obj("B/2", 3, "y")],
            [obj("B/1", 1, "x"), obj("B/2", 2, "z")],
            [obj("B/1", 1, "x"), obj("B/3", 2, "y")],
        ] {
            assert_ne!(group(&settings, &changed)[0].signature, a[0].signature);
        }
        let hex = a[0].signature.to_hex();
        assert_eq!(ItemSignature::from_hex(&hex), Some(a[0].signature));
        assert_eq!(ItemSignature::from_hex("zz"), None);
    }

    #[test]
    fn an_item_is_ready_only_after_the_quiet_period() {
        let settings = IntakeSettings::for_patron(1);
        let item = group(&settings, &[obj("Book.m4b", 1, "a")]).remove(0);
        let Step::Store(record) = step(None, Some(&item), 1_000, 60_000) else {
            panic!("new item is stored");
        };
        assert_eq!(record.state, TrackedState::Settling);
        assert_eq!(step(Some(&record), Some(&item), 60_999, 60_000), Step::Keep);
        let Step::Ready(queued) = step(Some(&record), Some(&item), 61_000, 60_000) else {
            panic!("settled item is ready");
        };
        assert_eq!(queued.state, TrackedState::Queued);
        // A queued item waits for its job, whatever the listing says.
        assert_eq!(step(Some(&queued), None, 70_000, 60_000), Step::Keep);
    }

    #[test]
    fn a_change_restarts_the_quiet_period() {
        let settings = IntakeSettings::for_patron(1);
        let first = group(&settings, &[obj("Book.m4b", 1, "a")]).remove(0);
        let grown = group(&settings, &[obj("Book.m4b", 2, "b")]).remove(0);
        let record = TrackedItem {
            state: TrackedState::Settling,
            signature: first.signature,
            quiet_since_ms: 0,
        };
        assert_eq!(
            step(Some(&record), Some(&grown), 100_000, 60_000),
            Step::Store(TrackedItem {
                state: TrackedState::Settling,
                signature: grown.signature,
                quiet_since_ms: 100_000,
            })
        );
        let done = TrackedItem {
            state: TrackedState::Done,
            ..record
        };
        assert_eq!(step(Some(&done), Some(&first), 100_000, 60_000), Step::Keep);
        assert!(matches!(
            step(Some(&done), Some(&grown), 100_000, 60_000),
            Step::Store(_)
        ));
        assert_eq!(step(Some(&done), None, 100_000, 60_000), Step::Forget);
    }

    #[test]
    fn an_oversized_item_never_becomes_ready() {
        let settings = IntakeSettings::for_patron(1);
        let keys: Vec<String> = (0..=MAX_ITEM_MEMBERS).map(|i| format!("Big/{i}")).collect();
        let mut grouper = ItemGrouper::new(&settings);
        for key in &keys {
            grouper.add(obj(key, 1, ""));
        }
        let item = grouper.finish().remove(0);
        assert_eq!(item.limit, Some(ItemLimit::TooManyMembers));
        assert_eq!(item.members.len(), MAX_ITEM_MEMBERS);
        let record = TrackedItem {
            state: TrackedState::Settling,
            signature: item.signature,
            quiet_since_ms: 0,
        };
        assert_eq!(step(Some(&record), Some(&item), u64::MAX, 1), Step::Keep);
    }

    #[test]
    fn a_member_past_the_cap_changes_the_signature() {
        let settings = IntakeSettings::for_patron(1);
        let keys: Vec<String> = (0..=MAX_ITEM_MEMBERS)
            .map(|i| format!("Big/{i:05}"))
            .collect();
        let mut at_cap = ItemGrouper::new(&settings);
        for key in &keys[..MAX_ITEM_MEMBERS] {
            at_cap.add(obj(key, 1, ""));
        }
        let at_cap = at_cap.finish().remove(0);
        assert_eq!(at_cap.limit, None);
        assert_eq!(at_cap.signature, signature_of(&at_cap.members, None));
        let mut over = ItemGrouper::new(&settings);
        for key in &keys {
            over.add(obj(key, 1, ""));
        }
        let over = over.finish().remove(0);
        assert_eq!(over.members, at_cap.members, "the extra member is not kept");
        assert_ne!(over.signature, at_cap.signature);
        // A record that settled at the cap restarts when the item grows.
        let record = TrackedItem {
            state: TrackedState::Settling,
            signature: at_cap.signature,
            quiet_since_ms: 0,
        };
        assert!(matches!(
            step(Some(&record), Some(&over), 1_000_000, 1),
            Step::Store(_)
        ));
    }
}
