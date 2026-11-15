//! Sample data for the hosted demo's library.
//!
//! A demo visitor lands in a library of their own, as its Owner. An empty
//! library shows little of what Bookclerk does with more than one person, so
//! before bookclerkd starts, the demo image runs this crate's binary once to
//! add a second library user, Sam, who:
//!
//! - has an account at each storefront the image ships (Audible, Libro.fm,
//!   Chirp and GraphicAudio), with well-known audiobooks from each already in
//!   the library;
//! - keeps a wishlist;
//! - owns a branch, "sams-nas", that shared part of its catalog and then
//!   went offline (its last heartbeat is days old).
//!
//! The titles are real audiobooks. Before seeding, the binary looks each one
//! up in its storefront's public catalog through the storefront plugin, the
//! search Discover uses, then fetches the matched title's full record (the
//! details call title dialogs use), and the seed writes the record the store
//! returned:
//! its product id, cover art, description and the rest, as a scan and
//! enrichment would. A title whose lookup fails (store unreachable, no
//! confident match) keeps the fields listed here under a made-up product id
//! prefixed `demo-`, so the demo still starts without the stores.
//!
//! Everything is written through the library store and the branch catalog
//! code the daemon uses, never raw SQL, so the seed follows the schema of
//! the commit it is built from. Nothing here signs in: Sam has no password
//! and no claim ticket, the store accounts hold no tokens, and the branch's
//! credential keys are placeholders that never verify a signature.

use anyhow::{bail, Context};
use bookclerk_library::control_plane::{
    accept_catalog_batch, begin_catalog, default_instance_for_key, ensure_hand_managed,
    ensure_plugin_deployment, load_deployment_for_node, replace_plugin_deployment, CatalogAccept,
    CatalogBegin, ConfigActor, DeploymentReplace, DesiredState,
};
use bookclerk_library::entities::{node_credentials, nodes};
use bookclerk_library::CatalogEnrichmentFields;
use bookclerk_library::{
    AcquireStatus, LibraryStore, NewBook, NewTitleRequest, RequestStatus, UserRole,
};
use bookclerk_node_proto::{CatalogFile, CatalogMetadata, CatalogTitle};
use bookclerk_source::CatalogHit;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

/// Login name of the sample library user; its presence marks a seeded library.
pub const SAMPLE_LOGIN: &str = "sam";

/// Display name of the sample library user.
const SAMPLE_NAME: &str = "Sam Rivera";

/// A storefront the demo image ships, by its plugin id (`plugin.toml` `id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Storefront {
    /// Plugin id the host enrolls the storefront under (its alias).
    pub id: &'static str,
    /// Name shown on Sam's account for it.
    pub label: &'static str,
}

/// Audible.
const AUDIBLE: Storefront = Storefront {
    id: "audible",
    label: "Sam's Audible",
};
/// Libro.fm.
const LIBRO: Storefront = Storefront {
    id: "libro",
    label: "Sam's Libro.fm",
};
/// Chirp.
const CHIRP: Storefront = Storefront {
    id: "chirp",
    label: "Sam's Chirp",
};
/// GraphicAudio.
const GRAPHIC_AUDIO: Storefront = Storefront {
    id: "graphicaudio",
    label: "Sam's GraphicAudio",
};

/// Every storefront Sam has an account at, in the order the accounts are added.
pub const STOREFRONTS: [Storefront; 4] = [AUDIBLE, LIBRO, CHIRP, GRAPHIC_AUDIO];

/// Plugins the demo runs besides the storefronts, by manifest id: what the
/// demo Worker enables as optional (`OPTIONAL_PLUGINS` in
/// `tools/demo/src/lifecycle.ts`). The seed enables them too, so a baked
/// image already has them, and one missing from the image is skipped.
pub const EXTRA_PLUGINS: [&str; 1] = ["covers"];

/// Account id of each of Sam's store accounts. `example.com` is reserved, so
/// it names no real person.
const SAMPLE_ACCOUNT: &str = "sam@example.com";

/// Node id of Sam's offline branch.
pub const SAMPLE_BRANCH: &str = "sams-nas";

/// How long ago Sam's branch last checked in, so every view shows it offline.
const BRANCH_SILENT_FOR: Duration = Duration::days(3);

/// One sample title: a real audiobook, with the fields used when its
/// storefront record cannot be fetched.
struct SampleTitle {
    /// Storefront the title was bought from.
    store: Storefront,
    /// Key of the title's [`StoreRecords`] entry, and its product id when the
    /// record is missing; prefixed `demo-` so it never matches a real
    /// storefront id.
    product_id: &'static str,
    /// Title of the work.
    title: &'static str,
    /// Author, as the storefront would list it.
    author: &'static str,
    /// Narrators, comma separated.
    narrators: &'static str,
    /// Series name and position, when the work is part of one.
    series: Option<(&'static str, &'static str)>,
    /// Running time in minutes, rounded.
    minutes: i64,
}

/// Titles already in the library from Sam's store accounts.
const LIBRARY: &[SampleTitle] = &[
    SampleTitle {
        store: AUDIBLE,
        product_id: "demo-project-hail-mary",
        title: "Project Hail Mary",
        author: "Andy Weir",
        narrators: "Ray Porter",
        series: None,
        minutes: 970,
    },
    SampleTitle {
        store: AUDIBLE,
        product_id: "demo-dune",
        title: "Dune",
        author: "Frank Herbert",
        narrators: "Scott Brick, Orlagh Cassidy, Euan Morton, Simon Vance, Ilyana Kadushin",
        series: Some(("Dune", "1")),
        minutes: 1262,
    },
    SampleTitle {
        store: AUDIBLE,
        product_id: "demo-the-way-of-kings",
        title: "The Way of Kings",
        author: "Brandon Sanderson",
        narrators: "Michael Kramer, Kate Reading",
        series: Some(("The Stormlight Archive", "1")),
        minutes: 2733,
    },
    SampleTitle {
        store: LIBRO,
        product_id: "demo-lessons-in-chemistry",
        title: "Lessons in Chemistry",
        author: "Bonnie Garmus",
        narrators: "Miranda Raison",
        series: None,
        minutes: 715,
    },
    SampleTitle {
        store: LIBRO,
        product_id: "demo-tomorrow-and-tomorrow",
        title: "Tomorrow, and Tomorrow, and Tomorrow",
        author: "Gabrielle Zevin",
        narrators: "Jennifer Kim, Julian Cihi",
        series: None,
        minutes: 824,
    },
    SampleTitle {
        store: LIBRO,
        product_id: "demo-the-midnight-library",
        title: "The Midnight Library",
        author: "Matt Haig",
        narrators: "Carey Mulligan",
        series: None,
        minutes: 530,
    },
    SampleTitle {
        store: CHIRP,
        product_id: "demo-where-the-crawdads-sing",
        title: "Where the Crawdads Sing",
        author: "Delia Owens",
        narrators: "Cassandra Campbell",
        series: None,
        minutes: 732,
    },
    SampleTitle {
        store: CHIRP,
        product_id: "demo-the-silent-patient",
        title: "The Silent Patient",
        author: "Alex Michaelides",
        narrators: "Jack Hawkins, Louise Brealey",
        series: None,
        minutes: 523,
    },
    SampleTitle {
        store: GRAPHIC_AUDIO,
        product_id: "demo-mistborn-1-part-1",
        title: "Mistborn: The Final Empire (1 of 3)",
        author: "Brandon Sanderson",
        narrators: "A full cast",
        series: Some(("Mistborn", "1")),
        minutes: 390,
    },
    SampleTitle {
        store: GRAPHIC_AUDIO,
        product_id: "demo-mistborn-1-part-2",
        title: "Mistborn: The Final Empire (2 of 3)",
        author: "Brandon Sanderson",
        narrators: "A full cast",
        series: Some(("Mistborn", "1")),
        minutes: 390,
    },
];

/// Titles Sam's branch shared before it went offline. Their files never
/// arrived, so they wait on the branch.
const BRANCH: &[SampleTitle] = &[
    SampleTitle {
        store: AUDIBLE,
        product_id: "demo-fourth-wing",
        title: "Fourth Wing",
        author: "Rebecca Yarros",
        narrators: "Rebecca Soler, Teddy Hamilton",
        series: Some(("The Empyrean", "1")),
        minutes: 1229,
    },
    SampleTitle {
        store: AUDIBLE,
        product_id: "demo-the-hobbit",
        title: "The Hobbit",
        author: "J. R. R. Tolkien",
        narrators: "Andy Serkis",
        series: None,
        minutes: 625,
    },
    SampleTitle {
        store: LIBRO,
        product_id: "demo-circe",
        title: "Circe",
        author: "Madeline Miller",
        narrators: "Perdita Weeks",
        series: None,
        minutes: 728,
    },
    SampleTitle {
        store: CHIRP,
        product_id: "demo-educated",
        title: "Educated",
        author: "Tara Westover",
        narrators: "Julia Whelan",
        series: None,
        minutes: 730,
    },
];

/// A work on Sam's wishlist.
struct WishTitle {
    /// Key of the work's [`StoreRecords`] entry.
    key: &'static str,
    /// Title of the work.
    title: &'static str,
    /// Author, as a storefront would list it.
    author: &'static str,
}

/// Works on Sam's wishlist. Their records come from Audible's catalog,
/// which every wishlist title is sold in.
const WISHLIST: &[WishTitle] = &[
    WishTitle {
        key: "demo-wish-wind-and-truth",
        title: "Wind and Truth",
        author: "Brandon Sanderson",
    },
    WishTitle {
        key: "demo-wish-iron-flame",
        title: "Iron Flame",
        author: "Rebecca Yarros",
    },
    WishTitle {
        key: "demo-wish-james",
        title: "James",
        author: "Percival Everett",
    },
    WishTitle {
        key: "demo-wish-the-women",
        title: "The Women",
        author: "Kristin Hannah",
    },
];

/// Storefront records found for the sample titles, by [`Lookup::key`].
pub type StoreRecords = BTreeMap<&'static str, CatalogHit>;

/// Storefront records for the sample titles as the storefront plugins
/// returned them, by [`Lookup::key`], committed in `records.json`.
///
/// The demo's own lookups run from a datacenter, where a storefront may
/// refuse or time out. These records stand in for a lookup that fails, so
/// every sample title is a real store record. Libro.fm's records come from
/// its app catalog's details, so they carry descriptions and prices. Refresh
/// them from a network the stores answer with `bookclerk-demo-seed
/// --write-records crates/bookclerk-demo-seed/records.json`.
const RECORDS_JSON: &str = include_str!("../records.json");

/// The committed storefront records (`records.json`) for the current
/// sample titles; entries for titles no longer sampled are dropped.
///
/// # Errors
///
/// Returns an error when `records.json` is not a JSON object of catalog
/// records.
pub fn committed_records() -> anyhow::Result<StoreRecords> {
    parse_records(RECORDS_JSON)
}

/// Parses a records file: a JSON object from [`Lookup::key`] to a catalog
/// record, keeping only keys of current sample titles.
///
/// # Errors
///
/// Returns an error when `json` is not such an object.
pub fn parse_records(json: &str) -> anyhow::Result<StoreRecords> {
    let parsed: BTreeMap<String, CatalogHit> =
        serde_json::from_str(json).context("parse the storefront records")?;
    Ok(lookups()
        .into_iter()
        .filter_map(|lookup| parsed.get(lookup.key).map(|hit| (lookup.key, hit.clone())))
        .collect())
}

/// Live lookups over the committed records: a title the live lookup found
/// keeps that record, and one it missed falls back to `committed` unless a
/// live record already holds that product id, so a stale committed record
/// never pushes out a fresh one. No two titles share a product id.
#[must_use]
pub fn merge_records(live: StoreRecords, committed: StoreRecords) -> StoreRecords {
    let mut records = distinct_records(live);
    let mut taken: BTreeSet<(&str, String)> = lookups()
        .into_iter()
        .filter_map(|lookup| {
            records
                .get(lookup.key)
                .map(|hit| (lookup.store, hit.product_id.clone()))
        })
        .collect();
    for lookup in lookups() {
        if records.contains_key(lookup.key) {
            continue;
        }
        if let Some(hit) = committed.get(lookup.key) {
            if taken.insert((lookup.store, hit.product_id.clone())) {
                records.insert(lookup.key, hit.clone());
            }
        }
    }
    records
}

/// `records` as the pretty-printed JSON `records.json` holds, one key per
/// title in sorted order, so a refresh diffs cleanly.
///
/// # Errors
///
/// Returns an error when a record cannot be serialized.
pub fn records_json(records: &StoreRecords) -> anyhow::Result<String> {
    let mut json = serde_json::to_string_pretty(records).context("write the records")?;
    json.push('\n');
    Ok(json)
}

/// One title to look up in a storefront's public catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lookup {
    /// Key the record is stored under in [`StoreRecords`].
    pub key: &'static str,
    /// Plugin id ([`Storefront::id`]) of the storefront to search.
    pub store: &'static str,
    /// Title to search for and match.
    pub title: &'static str,
    /// Author the matching record must list.
    pub author: &'static str,
}

/// Every title the seed writes, as catalog lookups: the library, the
/// branch and the wishlist.
#[must_use]
pub fn lookups() -> Vec<Lookup> {
    LIBRARY
        .iter()
        .chain(BRANCH)
        .map(|sample| Lookup {
            key: sample.product_id,
            store: sample.store.id,
            title: sample.title,
            author: sample.author,
        })
        .chain(WISHLIST.iter().map(|wish| Lookup {
            key: wish.key,
            store: AUDIBLE.id,
            title: wish.title,
            author: wish.author,
        }))
        .collect()
}

/// Lowercase words of `text`, letters and digits only, so punctuation and
/// spacing differences between storefronts do not matter.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// `text` without a trailing `: subtitle` or `(…)` part.
fn main_title(text: &str) -> &str {
    let end = text.find([':', '(']).unwrap_or(text.len());
    text[..end].trim()
}

/// How well a catalog title matches the wanted one; `None` when it does not.
///
/// 3 = same words; 2 = same words once the catalog's subtitle or
/// parenthetical is dropped (a store that folds "Book One" into the title);
/// 1 = every wanted word appears in order and the wanted title has at least
/// four words (a store that adds a series number, like
/// "Mistborn 1: The Final Empire (1 of 3)"). Short titles must match more
/// closely so "Dune" never matches "Dune Messiah".
fn title_score(wanted: &str, found: &str) -> Option<u8> {
    let want = words(wanted);
    let got = words(found);
    if want == got {
        return Some(3);
    }
    if want == words(main_title(found)) {
        return Some(2);
    }
    let mut rest = got.iter();
    let in_order = want.iter().all(|w| rest.any(|g| g == w));
    (want.len() >= 4 && in_order && got.len() <= want.len() + 2).then_some(1)
}

/// Whether `authors` (a storefront's comma- or `&`-separated list) names
/// `wanted`: some entry has the same surname and a first name with the same
/// initial, so "J. R. R. Tolkien" matches "J.R.R. Tolkien" but "Brian
/// Herbert" does not match "Frank Herbert".
fn lists_author(authors: &str, wanted: &str) -> bool {
    let want = words(wanted);
    let (Some(first), Some(last)) = (want.first(), want.last()) else {
        return false;
    };
    authors
        .split([',', '&', ';'])
        .flat_map(|entry| entry.split(" and "))
        .map(words)
        .any(|name| {
            name.last() == Some(last)
                && name
                    .first()
                    .is_some_and(|f| f.chars().next() == first.chars().next())
        })
}

/// The catalog search text for `lookup`: the title up to any `(…)` part,
/// then the author's surname.
///
/// GraphicAudio's catalog search finds a title only from its plain words:
/// punctuation or an author name makes it fall back to an alphabetical
/// listing of the whole catalog. Its query is the title's words alone.
#[must_use]
pub fn search_query(lookup: &Lookup) -> String {
    let title = lookup
        .title
        .split('(')
        .next()
        .unwrap_or(lookup.title)
        .trim();
    if lookup.store == GRAPHIC_AUDIO.id {
        return words(title).join(" ");
    }
    match words(lookup.author).pop() {
        Some(surname) => format!("{title} {surname}"),
        None => title.to_owned(),
    }
}

/// The search hit that is the wanted title by the wanted author, or `None`.
///
/// A hit that lists authors must list the wanted one (same surname and
/// first initial). A GraphicAudio hit without authors is judged on its
/// title alone, because that store never returns them; any other store's
/// authorless hit is refused. Among
/// matching hits the closest title wins, then one already released (a
/// pre-order of a new recording is not the edition a listener owns, by
/// `now`), then the store's own ranking.
#[must_use]
pub fn best_match<'a>(
    lookup: &Lookup,
    hits: &'a [CatalogHit],
    now: DateTime<Utc>,
) -> Option<&'a CatalogHit> {
    hits.iter()
        .filter(|hit| !hit.product_id.trim().is_empty())
        .filter(|hit| match present(hit.authors().as_ref()) {
            Some(authors) => lists_author(&authors, lookup.author),
            None => lookup.store == GRAPHIC_AUDIO.id,
        })
        .filter_map(|hit| {
            title_score(lookup.title, &hit.title).map(|score| (score, released(hit, now), hit))
        })
        // `max_by_key` keeps the last of equal keys; reverse so the store's
        // first-ranked hit wins a tie.
        .rev()
        .max_by_key(|(score, released, _)| (*score, *released))
        .map(|(_, _, hit)| hit)
}

/// Whether `hit` is out by `now`; a hit without a readable release date
/// counts as released.
fn released(hit: &CatalogHit, now: DateTime<Utc>) -> bool {
    hit.published_at
        .as_deref()
        .and_then(release_date)
        .is_none_or(|at| at <= now)
}

/// `records` without entries whose product id another title already took,
/// so two sample titles never become one book (the first, in
/// [`lookups`] order, keeps it).
#[must_use]
pub fn distinct_records(mut records: StoreRecords) -> StoreRecords {
    let mut taken = BTreeSet::new();
    for lookup in lookups() {
        let duplicate = records
            .get(lookup.key)
            .is_some_and(|hit| !taken.insert((lookup.store, hit.product_id.clone())));
        if duplicate {
            records.remove(lookup.key);
        }
    }
    records
}

/// Parses a storefront release date (RFC 3339 or `YYYY-MM-DD`).
fn release_date(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    DateTime::parse_from_rfc3339(raw)
        .map(|at| at.to_utc())
        .ok()
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(raw.get(..10)?, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
                .map(|at| at.and_utc())
        })
}

/// `value` when it holds more than whitespace.
fn present(value: Option<&String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty()).cloned()
}

/// What [`seed`] wrote, for the log and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedReport {
    /// Library user id of Sam.
    pub user_id: i64,
    /// Books added from Sam's store accounts.
    pub books: usize,
    /// Titles Sam's branch shared.
    pub branch_titles: u64,
    /// Wishlist items added for Sam.
    pub wishlist: usize,
}

/// Outcome of [`seed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seeded {
    /// The sample data was written.
    Wrote(SeedReport),
    /// The library already has the sample user; nothing was written.
    AlreadySeeded,
}

/// Adds the sample library user, their books, wishlist and offline branch.
///
/// Runs once on a fresh demo library, before bookclerkd starts. A library
/// that already has [`SAMPLE_LOGIN`] is left alone, so running it twice
/// changes nothing.
///
/// # Arguments
///
/// * `store` - The open library.
/// * `cluster_id` - The library's cluster id, recorded on the branch's node row.
/// * `plugin_keys` - PluginKey of each enrolled storefront, by plugin id
///   ([`Storefront::id`]). A storefront missing from it gets no account and
///   its titles are left out, so an image without one still gets the rest.
/// * `records` - Storefront records for the titles, by [`Lookup::key`]
///   (pass them through [`distinct_records`]). A title without one keeps
///   the fields listed in this crate.
/// * `when` - The time the library was seeded; purchase dates and the
///   branch's last heartbeat count back from it.
///
/// # Returns
///
/// What was written, or [`Seeded::AlreadySeeded`].
///
/// # Errors
///
/// Returns an error when a write fails or the branch catalog refuses the
/// branch, which means the library code changed what makes a branch trusted.
pub async fn seed(
    store: &LibraryStore,
    cluster_id: &str,
    plugin_keys: &BTreeMap<&str, String>,
    records: &StoreRecords,
    when: DateTime<Utc>,
) -> anyhow::Result<Seeded> {
    if store.get_user_by_login_name(SAMPLE_LOGIN).await?.is_some() {
        return Ok(Seeded::AlreadySeeded);
    }
    let user = store
        .create_user_with_profile(
            UserRole::Member,
            Some(SAMPLE_NAME),
            Some(SAMPLE_LOGIN),
            None,
            None,
        )
        .await
        .context("create the sample user")?;
    let identity = store
        .ensure_local_portal_identity(user.id, Some(SAMPLE_NAME))
        .await
        .context("create the sample user's identity")?;

    for front in STOREFRONTS {
        let Some(key) = plugin_keys.get(front.id) else {
            continue;
        };
        store
            .upsert_account(
                key,
                SAMPLE_ACCOUNT,
                "us",
                Some(front.label),
                false,
                front.id,
            )
            .await
            .with_context(|| format!("add the sample {} account", front.id))?;
        store
            .link_account(identity.id, key, SAMPLE_ACCOUNT, front.id)
            .await
            .with_context(|| format!("link the sample {} account", front.id))?;
    }
    let bought = when - Duration::days(40);
    let mut books = 0;
    for (i, sample) in LIBRARY.iter().enumerate() {
        let Some(key) = plugin_keys.get(sample.store.id) else {
            continue;
        };
        let hit = records.get(sample.product_id);
        let mut book = new_book(sample, hit);
        book.plugin_key = key.clone();
        book.purchased_at = Some(bought + Duration::days(i64::try_from(i)? * 3));
        let record = store
            .upsert_book(&book)
            .await
            .with_context(|| format!("add {}", sample.title))?;
        store
            .set_acquire_status(
                &record.uuid,
                SAMPLE_ACCOUNT,
                AcquireStatus::Acquired,
                Some(&storage_key(sample)),
                None,
            )
            .await
            .with_context(|| format!("mark {} acquired", sample.title))?;
        if let Some(hit) = hit {
            store
                .update_catalog_enrichment(&record.uuid, &enrichment(sample.store.id, hit, when))
                .await
                .with_context(|| format!("add the store record of {}", sample.title))?;
        }
        books += 1;
    }

    for wish in WISHLIST {
        let hit = records.get(wish.key);
        store
            .create_title_request(&NewTitleRequest {
                uuid: None,
                identity_id: Some(identity.id),
                title: hit.map_or(wish.title, |h| h.title.as_str()).to_owned(),
                authors: Some(
                    hit.and_then(|h| present(h.authors().as_ref()))
                        .unwrap_or_else(|| wish.author.to_owned()),
                ),
                asin: hit.and_then(|h| present(h.asin().as_ref())),
                isbn: hit.and_then(|h| present(h.isbn().as_ref())),
                notes: None,
                status: RequestStatus::Open,
                resolved_edition_id: None,
                cover_url: hit.and_then(|h| present(h.cover_url.as_ref())),
            })
            .await
            .with_context(|| format!("wish for {}", wish.title))?;
    }

    let branch_titles = add_branch(store, cluster_id, user.id, plugin_keys, records, when).await?;
    Ok(Seeded::Wrote(SeedReport {
        user_id: user.id,
        books,
        branch_titles,
        wishlist: WISHLIST.len(),
    }))
}

/// Where the library keeps a sample title's audio, as an acquire would name it.
fn storage_key(sample: &SampleTitle) -> String {
    format!("{}/{}/{}.m4b", sample.author, sample.title, sample.title)
}

/// The library row a scan of Sam's account would write for `sample`: from
/// the storefront record `hit` when there is one, else from the sample.
fn new_book(sample: &SampleTitle, hit: Option<&CatalogHit>) -> NewBook {
    let Some(hit) = hit else {
        let mut book = NewBook::minimal(sample.product_id, SAMPLE_ACCOUNT, "us", sample.title);
        book.asin = None;
        book.source = sample.store.id.to_owned();
        book.authors = Some(sample.author.to_owned());
        book.narrators = Some(sample.narrators.to_owned());
        book.series = sample.series.map(|(name, _)| name.to_owned());
        book.series_index = sample.series.map(|(_, index)| index.to_owned());
        book.length_minutes = Some(sample.minutes);
        return book;
    };
    let mut book = NewBook::minimal(&hit.product_id, SAMPLE_ACCOUNT, "us", &hit.title);
    book.source = sample.store.id.to_owned();
    book.asin = present(hit.asin().as_ref());
    book.isbn = present(hit.isbn().as_ref());
    book.authors = present(hit.authors().as_ref()).or_else(|| Some(sample.author.to_owned()));
    book.narrators =
        present(hit.narrators().as_ref()).or_else(|| Some(sample.narrators.to_owned()));
    // A store that leaves the series out keeps the sample's.
    let (series, series_index) = match present(hit.series().as_ref()) {
        Some(series) => (Some(series), present(hit.series_index().as_ref())),
        None => (
            sample.series.map(|(name, _)| name.to_owned()),
            sample.series.map(|(_, index)| index.to_owned()),
        ),
    };
    book.series = series;
    book.series_index = series_index;
    book.subtitle = present(hit.subtitle.as_ref());
    book.publisher = present(hit.publisher.as_ref());
    book.categories = present(hit.categories.as_ref());
    book.length_minutes = hit.length_minutes.or(Some(sample.minutes));
    book.published_at = hit.published_at.as_deref().and_then(release_date);
    book
}

/// The enrichment fields of a storefront record: what a scan does not
/// carry but the record has (cover art, description, language).
fn enrichment(store: &str, hit: &CatalogHit, when: DateTime<Utc>) -> CatalogEnrichmentFields {
    CatalogEnrichmentFields {
        description: present(hit.description.as_ref()),
        language: present(hit.language.as_ref()),
        cover_url: present(hit.cover_url.as_ref()),
        subjects: None,
        categories: present(hit.categories.as_ref()),
        enrich_source: Some(store.to_owned()),
        enrich_confidence: Some(1.0),
        enrich_updated_at: Some(when),
    }
}

/// Enrolls Sam's branch, lets it share the [`BRANCH`] titles of the enrolled
/// storefronts (`plugin_keys`) through the same catalog code a live branch's
/// sync uses, and leaves it silent for [`BRANCH_SILENT_FOR`].
///
/// # Returns
///
/// The number of titles the catalog recorded.
async fn add_branch(
    store: &LibraryStore,
    cluster_id: &str,
    owner: i64,
    plugin_keys: &BTreeMap<&str, String>,
    records: &StoreRecords,
    when: DateTime<Utc>,
) -> anyhow::Result<u64> {
    let last_seen = (when - BRANCH_SILENT_FOR).to_rfc3339();
    let enrolled = (when - Duration::days(30)).to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(SAMPLE_BRANCH.into()),
        cluster_id: Set(cluster_id.into()),
        created_at: Set(enrolled.clone()),
        incarnation: Set("demo".into()),
        heartbeat_at: Set(last_seen.clone()),
        software_version: Set(env!("CARGO_PKG_VERSION").into()),
        schema_state: Set("current".into()),
        compatible: Set(1),
        target: Set(String::new()),
        labels_json: Set("[]".into()),
        kind: Set("host".into()),
        owner_user_id: Set(Some(owner)),
        ..Default::default()
    }
    .insert(store.db())
    .await
    .context("add the sample branch")?;
    // Placeholder keys: the branch never connects, so they never verify
    // anything; the row is what makes the library treat it as a branch.
    node_credentials::ActiveModel {
        node_id: Set(SAMPLE_BRANCH.into()),
        sign_public_key: Set(vec![0; 32]),
        box_public_key: Set(vec![0; 32]),
        link_id: Set(format!("demo-{SAMPLE_BRANCH}")),
        enrolled_at: Set(enrolled),
        revoked_at: Set(None),
        last_connected_at: Set(Some(last_seen.clone())),
    }
    .insert(store.db())
    .await
    .context("trust the sample branch")?;

    let opened = match begin_catalog(store, SAMPLE_BRANCH, true, false).await? {
        CatalogBegin::Opened(opened) => opened,
        CatalogBegin::Refused(reason) => bail!("the library refused the sample branch: {reason}"),
    };
    // Like the library, only titles of storefronts the image enrolled.
    let titles: Vec<CatalogTitle> = BRANCH
        .iter()
        .filter(|sample| plugin_keys.contains_key(sample.store.id))
        .map(|sample| catalog_title(sample, records.get(sample.product_id)))
        .collect();
    let upserted = match accept_catalog_batch(
        store,
        SAMPLE_BRANCH,
        opened.fence,
        &opened.cursor,
        true,
        &titles,
    )
    .await?
    {
        CatalogAccept::Accepted { upserted, .. } => upserted,
        other => bail!("the library did not accept the sample branch's titles: {other:?}"),
    };
    // Catalog sync does not touch the heartbeat; set it last so nothing
    // above refreshes it.
    nodes::Entity::update_many()
        .col_expr(
            nodes::Column::HeartbeatAt,
            sea_orm::sea_query::Expr::value(last_seen.clone()),
        )
        .col_expr(
            nodes::Column::CatalogSyncedAt,
            sea_orm::sea_query::Expr::value(Some(last_seen)),
        )
        .filter(nodes::Column::NodeId.eq(SAMPLE_BRANCH))
        .exec(store.db())
        .await
        .context("age the sample branch")?;
    Ok(upserted)
}

/// The catalog entry a branch would send for `sample`, from the storefront
/// record `hit` when there is one.
fn catalog_title(sample: &SampleTitle, hit: Option<&CatalogHit>) -> CatalogTitle {
    let key = storage_key(sample);
    let book = new_book(sample, hit);
    CatalogTitle {
        store: sample.store.id.into(),
        product_id: book.product_id,
        marketplace: {
            // A storefront's own spelling (`en-US`, padded) is sent as no
            // marketplace rather than refusing the whole sample batch.
            let market = book.marketplace.trim().to_ascii_lowercase();
            if bookclerk_node_proto::is_marketplace(&market) {
                market
            } else {
                String::new()
            }
        },
        metadata: CatalogMetadata {
            title: book.title,
            subtitle: book.subtitle,
            authors: book.authors,
            narrators: book.narrators,
            series: book.series,
            series_index: book.series_index,
            asin: book.asin,
            isbn: book.isbn,
            publisher: book.publisher,
            length_minutes: book.length_minutes,
            language: hit.and_then(|h| present(h.language.as_ref())),
            published_at: book.published_at.map(|at| at.to_rfc3339()),
            ..CatalogMetadata::default()
        },
        primary_key: key.clone(),
        pdf_key: None,
        files: vec![CatalogFile {
            key,
            size: u64::try_from(sample.minutes)
                .unwrap_or(0)
                .saturating_mul(480_000),
            content_type: Some("audio/mp4".into()),
        }],
    }
}

/// Replaces a search hit with its storefront's full catalog record (the
/// details call Discover makes for title dialogs), so a store whose search
/// rows are lean, such as Libro.fm, still seeds a description, publisher,
/// genres and price.
///
/// # Arguments
///
/// * `details` - The storefront's catalog-detail lookup for a product id
///   (the source's `catalog_detail`); only this call is made.
/// * `hit` - The search row [`best_match`] chose.
/// * `deadline` - When the lookup budget runs out.
///
/// # Returns
///
/// The full record under `hit`'s product id, or `hit` unchanged when the
/// details call fails, times out, finds nothing, or returns a different
/// product than the search matched (neither the same product id nor the
/// same ISBN).
pub async fn with_details<F, Fut>(
    details: F,
    hit: CatalogHit,
    deadline: tokio::time::Instant,
) -> CatalogHit
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = bookclerk_source::Result<Option<CatalogHit>>>,
{
    let lookup = tokio::time::timeout_at(deadline, details(hit.product_id.clone()));
    match lookup.await {
        Ok(Ok(Some(mut full))) if same_product(&full, &hit) => {
            // Keep the id the search matched, so records stay keyed alike.
            full.product_id = hit.product_id;
            full
        }
        Ok(Ok(_)) => hit,
        Ok(Err(err)) => {
            tracing::warn!(product = %hit.product_id, %err, "catalog details failed");
            hit
        }
        Err(_) => {
            tracing::warn!(product = %hit.product_id, "catalog details timed out");
            hit
        }
    }
}

/// Whether a details record describes the same product as a search row: the
/// same product id, or the same ISBN when a store keys its search rows and
/// details differently (an ISBN on one, a slug on the other).
fn same_product(full: &CatalogHit, hit: &CatalogHit) -> bool {
    full.product_id == hit.product_id || hit.isbn().is_some_and(|isbn| full.isbn() == Some(isbn))
}

/// What one run of the seed binary does, from its command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// No arguments: seed the library.
    Seed,
    /// `--write-records <path>`: look the sample titles up and write the
    /// records found to the path, seeding nothing.
    WriteRecords(std::path::PathBuf),
    /// `--bake-finish`: turn the seeded files dir into the template a demo
    /// image ships (`packaging/demo/bake-seed.sh`), removing every secret
    /// and runtime file a container must make for itself.
    BakeFinish,
}

/// Parses the seed binary's arguments into a [`Mode`].
///
/// # Arguments
///
/// * `args` - The command-line arguments after the program name.
///
/// # Returns
///
/// The mode the arguments ask for.
///
/// # Errors
///
/// Returns an error for any other argument, a missing path, or an extra
/// argument.
pub fn parse_mode(mut args: impl Iterator<Item = String>) -> anyhow::Result<Mode> {
    let mode = match args.next() {
        None => return Ok(Mode::Seed),
        Some(flag) if flag == "--write-records" => match args.next() {
            Some(path) => Mode::WriteRecords(path.into()),
            None => bail!("--write-records needs a path"),
        },
        Some(flag) if flag == "--bake-finish" => Mode::BakeFinish,
        Some(other) => bail!("unexpected argument `{other}`"),
    };
    match args.next() {
        None => Ok(mode),
        Some(extra) => bail!("unexpected argument `{extra}`"),
    }
}

/// SQLite side files next to `library.db` that a finished bake must not
/// carry with data in them.
const SQLITE_SIDE_FILES: [&str; 3] = ["library.db-journal", "library.db-wal", "library.db-shm"];

/// Deletes the files a seeded files dir holds that every demo container
/// must make for itself, so the dir can be baked into the image.
///
/// Removes `master.key` (the first daemon of each container mints and wraps
/// its own), the covers cache's URL-signing key (`covers/token.key` under
/// the cache dir), `logs/`, each plugin's `session-*` dirs under
/// `plugin-state/`, any joined library's plugin state
/// (`plugin-state/link-*`), and empty SQLite side files. Anything already absent
/// counts as removed.
///
/// # Arguments
///
/// * `files_dir` - The seeded `$BOOKCLERK_FILES_DIR`.
/// * `cache_dir` - Its cache dir (`paths.cache_dir`).
///
/// # Errors
///
/// Returns an error when a file cannot be removed, or when a SQLite side
/// file still holds data: it would carry writes the copied `library.db`
/// cannot see, so bookclerkd must stop first.
pub fn clear_install_files(files_dir: &Path, cache_dir: &Path) -> anyhow::Result<()> {
    remove_file_if_present(&bookclerk_library::master_key_path(files_dir))?;
    remove_file_if_present(&cache_dir.join("covers").join("token.key"))?;
    remove_dir_if_present(&files_dir.join("logs"))?;
    let state = files_dir.join("plugin-state");
    if state.is_dir() {
        for plugin in std::fs::read_dir(&state).context("list plugin-state")? {
            let plugin = plugin?;
            if plugin
                .file_name()
                .to_str()
                .is_some_and(bookclerk_plugin_host::LinkStateDir::is_leaf)
            {
                // A joined library's plugin state never belongs in a bake.
                remove_dir_if_present(&plugin.path())?;
                continue;
            }
            for entry in std::fs::read_dir(plugin.path()).context("list a plugin's state")? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with("session-") {
                    remove_dir_if_present(&entry.path())?;
                }
            }
        }
    }
    for side in SQLITE_SIDE_FILES {
        let path = files_dir.join(side);
        let holds_data = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0);
        if holds_data {
            bail!("{side} still holds data; stop bookclerkd before finishing the bake");
        }
        remove_file_if_present(&path)?;
    }
    Ok(())
}

/// Removes a file, treating an absent one as removed.
///
/// # Errors
///
/// Returns any other I/O error.
fn remove_file_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Removes a directory tree, treating an absent one as removed.
///
/// # Errors
///
/// Returns any other I/O error.
fn remove_dir_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Deploys a plugin's default instance on `node_id` as the operator, as
/// `plugins deploy` does; an instance already present is left alone.
///
/// # Arguments
///
/// * `store` - The library.
/// * `node_id` - The node the instance should run on (this host's).
/// * `key` - The plugin's canonical key.
///
/// # Errors
///
/// Returns an error when the plugin has no default instance, the scheduler
/// manages it, or a deployment write fails or races another writer.
pub async fn deploy_on_node(store: &LibraryStore, node_id: &str, key: &str) -> anyhow::Result<()> {
    let actor = &ConfigActor::Operator {
        id: "demo-seed".into(),
    };
    let instance = default_instance_for_key(store, key)
        .await?
        .with_context(|| format!("no default instance for {key}"))?;
    // A scheduler-placed instance is not the seed's to deploy, as for
    // `plugins deploy`.
    ensure_hand_managed(store, &instance.id).await?;
    let present = DesiredState::operator(true);
    let current = match load_deployment_for_node(store, &instance.id, node_id).await? {
        Some(current) => current,
        None => ensure_plugin_deployment(store, actor, &instance.id, node_id, present).await?,
    };
    if current.desired == present {
        return Ok(());
    }
    let operation = format!(
        "demo-seed-deploy-{}-{}",
        instance.id,
        Utc::now().timestamp_micros()
    );
    match replace_plugin_deployment(
        store,
        actor,
        &current.deployment_id,
        current.revision,
        present,
        &operation,
    )
    .await?
    {
        DeploymentReplace::Applied(_) | DeploymentReplace::Replayed { .. } => Ok(()),
        DeploymentReplace::Conflict { current_revision } => {
            bail!("deployment of {key} changed concurrently (revision {current_revision})")
        }
    }
}
