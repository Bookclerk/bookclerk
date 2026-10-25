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
//! The titles, authors and narrators are real so the library looks like one
//! a person would keep; product ids are made up (prefixed `demo-`) so none
//! claims to be a particular storefront listing.
//!
//! Everything is written through the library store and the branch catalog
//! code the daemon uses, never raw SQL, so the seed follows the schema of
//! the commit it is built from. Nothing here signs in: Sam has no password
//! and no claim ticket, the store accounts hold no tokens, and the branch's
//! credential keys are placeholders that never verify a signature.

use anyhow::{bail, Context};
use bookclerk_library::control_plane::{
    accept_catalog_batch, begin_catalog, CatalogAccept, CatalogBegin,
};
use bookclerk_library::entities::{node_credentials, nodes};
use bookclerk_library::{
    AcquireStatus, LibraryStore, NewBook, NewTitleRequest, RequestStatus, UserRole,
};
use bookclerk_node_proto::{CatalogFile, CatalogMetadata, CatalogTitle};
use std::collections::BTreeMap;

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

/// Account id of each of Sam's store accounts. `example.com` is reserved, so
/// it names no real person.
const SAMPLE_ACCOUNT: &str = "sam@example.com";

/// Node id of Sam's offline branch.
pub const SAMPLE_BRANCH: &str = "sams-nas";

/// How long ago Sam's branch last checked in, so every view shows it offline.
const BRANCH_SILENT_FOR: Duration = Duration::days(3);

/// One sample title: a real audiobook under a made-up product id.
struct SampleTitle {
    /// Storefront the title was bought from.
    store: Storefront,
    /// Product id, prefixed `demo-` so it can never match a real storefront id.
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

/// Works on Sam's wishlist: `(title, author)`.
const WISHLIST: &[(&str, &str)] = &[
    ("Wind and Truth", "Brandon Sanderson"),
    ("Iron Flame", "Rebecca Yarros"),
    ("James", "Percival Everett"),
    ("The Women", "Kristin Hannah"),
];

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
        let mut book = NewBook::minimal(sample.product_id, SAMPLE_ACCOUNT, "us", sample.title);
        book.asin = None;
        book.plugin_key = key.clone();
        book.source = sample.store.id.to_owned();
        book.authors = Some(sample.author.to_owned());
        book.narrators = Some(sample.narrators.to_owned());
        book.series = sample.series.map(|(name, _)| name.to_owned());
        book.series_index = sample.series.map(|(_, index)| index.to_owned());
        book.length_minutes = Some(sample.minutes);
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
        books += 1;
    }

    for (title, author) in WISHLIST {
        store
            .create_title_request(&NewTitleRequest {
                uuid: None,
                identity_id: Some(identity.id),
                title: (*title).to_owned(),
                authors: Some((*author).to_owned()),
                asin: None,
                isbn: None,
                notes: None,
                status: RequestStatus::Open,
                // Empty: the store derives the work key from title and author.
                work_key: String::new(),
                work_id: None,
                resolved_book_uuid: None,
                cover_url: None,
            })
            .await
            .with_context(|| format!("wish for {title}"))?;
    }

    let branch_titles = add_branch(store, cluster_id, user.id, when).await?;
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

/// Enrolls Sam's branch, lets it share [`BRANCH`] through the same catalog
/// code a live branch's sync uses, and leaves it silent for
/// [`BRANCH_SILENT_FOR`].
///
/// # Returns
///
/// The number of titles the catalog recorded.
async fn add_branch(
    store: &LibraryStore,
    cluster_id: &str,
    owner: i64,
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
    let titles: Vec<CatalogTitle> = BRANCH.iter().map(catalog_title).collect();
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

/// The catalog entry a branch would send for `sample`.
fn catalog_title(sample: &SampleTitle) -> CatalogTitle {
    let key = storage_key(sample);
    CatalogTitle {
        store: sample.store.id.into(),
        product_id: sample.product_id.into(),
        metadata: CatalogMetadata {
            title: sample.title.into(),
            authors: Some(sample.author.into()),
            narrators: Some(sample.narrators.into()),
            series: sample.series.map(|(name, _)| name.into()),
            series_index: sample.series.map(|(_, index)| index.into()),
            length_minutes: Some(sample.minutes),
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
