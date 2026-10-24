//! Sample data for the hosted demo's library.
//!
//! A demo visitor lands in a library of their own, as its Owner. An empty
//! library shows little of what Bookclerk does with more than one person, so
//! before bookclerkd starts, the demo image runs this crate's binary once to
//! add a second library user, Sam, who:
//!
//! - has a Libro.fm account linked, with public-domain titles already in the
//!   library;
//! - keeps a wishlist;
//! - owns a branch, "sams-nas", that shared part of its catalog and then
//!   went offline (its last heartbeat is days old).
//!
//! Everything is written through the library store and the branch catalog
//! code the daemon uses, never raw SQL, so the seed follows the schema of
//! the commit it is built from. Nothing here signs in: Sam has no password
//! and no claim ticket, and the branch's credential keys are placeholders
//! that never verify a signature.

use anyhow::{bail, Context};
use bookclerk_library::control_plane::{
    accept_catalog_batch, begin_catalog, CatalogAccept, CatalogBegin,
};
use bookclerk_library::entities::{node_credentials, nodes};
use bookclerk_library::{
    AcquireStatus, LibraryStore, NewBook, NewTitleRequest, RequestStatus, UserRole,
};
use bookclerk_node_proto::{CatalogFile, CatalogMetadata, CatalogTitle};
use chrono::{Duration, Utc};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

/// Login name of the sample library user; its presence marks a seeded library.
pub const SAMPLE_LOGIN: &str = "sam";

/// Display name of the sample library user.
const SAMPLE_NAME: &str = "Sam Rivera";

/// Plugin id (`plugin.toml` `id`) of the storefront Sam's books come from.
pub const SAMPLE_STORE: &str = "libro";

/// Account id of Sam's Libro.fm account. `example.com` is reserved, so it
/// names no real person.
const SAMPLE_ACCOUNT: &str = "sam@example.com";

/// Node id of Sam's offline branch.
pub const SAMPLE_BRANCH: &str = "sams-nas";

/// How long ago Sam's branch last checked in, so every view shows it offline.
const BRANCH_SILENT_FOR: Duration = Duration::days(3);

/// One sample title: a public-domain work, so nothing here advertises or
/// misattributes a real storefront product.
struct SampleTitle {
    /// Product id, prefixed `demo-` so it can never match a real storefront id.
    product_id: &'static str,
    /// Title of the work.
    title: &'static str,
    /// Author, as the storefront would list it.
    author: &'static str,
    /// Series name, when the work is part of one.
    series: Option<(&'static str, &'static str)>,
    /// Running time in minutes.
    minutes: i64,
}

/// Titles already in the library from Sam's Libro.fm account.
const LIBRARY: &[SampleTitle] = &[
    SampleTitle {
        product_id: "demo-pride-and-prejudice",
        title: "Pride and Prejudice",
        author: "Jane Austen",
        series: None,
        minutes: 709,
    },
    SampleTitle {
        product_id: "demo-persuasion",
        title: "Persuasion",
        author: "Jane Austen",
        series: None,
        minutes: 513,
    },
    SampleTitle {
        product_id: "demo-frankenstein",
        title: "Frankenstein",
        author: "Mary Shelley",
        series: None,
        minutes: 520,
    },
    SampleTitle {
        product_id: "demo-moby-dick",
        title: "Moby-Dick",
        author: "Herman Melville",
        series: None,
        minutes: 1458,
    },
    SampleTitle {
        product_id: "demo-study-in-scarlet",
        title: "A Study in Scarlet",
        author: "Arthur Conan Doyle",
        series: Some(("Sherlock Holmes", "1")),
        minutes: 293,
    },
    SampleTitle {
        product_id: "demo-sign-of-the-four",
        title: "The Sign of the Four",
        author: "Arthur Conan Doyle",
        series: Some(("Sherlock Holmes", "2")),
        minutes: 270,
    },
    SampleTitle {
        product_id: "demo-hound-of-the-baskervilles",
        title: "The Hound of the Baskervilles",
        author: "Arthur Conan Doyle",
        series: Some(("Sherlock Holmes", "5")),
        minutes: 382,
    },
    SampleTitle {
        product_id: "demo-time-machine",
        title: "The Time Machine",
        author: "H. G. Wells",
        series: None,
        minutes: 211,
    },
    SampleTitle {
        product_id: "demo-around-the-world",
        title: "Around the World in Eighty Days",
        author: "Jules Verne",
        series: None,
        minutes: 431,
    },
    SampleTitle {
        product_id: "demo-wizard-of-oz",
        title: "The Wonderful Wizard of Oz",
        author: "L. Frank Baum",
        series: Some(("Oz", "1")),
        minutes: 232,
    },
];

/// Titles Sam's branch shared before it went offline. Their files never
/// arrived, so they wait on the branch.
const BRANCH: &[SampleTitle] = &[
    SampleTitle {
        product_id: "demo-jane-eyre",
        title: "Jane Eyre",
        author: "Charlotte Brontë",
        series: None,
        minutes: 1131,
    },
    SampleTitle {
        product_id: "demo-dracula",
        title: "Dracula",
        author: "Bram Stoker",
        series: None,
        minutes: 941,
    },
    SampleTitle {
        product_id: "demo-treasure-island",
        title: "Treasure Island",
        author: "Robert Louis Stevenson",
        series: None,
        minutes: 410,
    },
    SampleTitle {
        product_id: "demo-little-women",
        title: "Little Women",
        author: "Louisa May Alcott",
        series: None,
        minutes: 1093,
    },
];

/// Works on Sam's wishlist: `(title, author)`.
const WISHLIST: &[(&str, &str)] = &[
    ("Middlemarch", "George Eliot"),
    ("The Count of Monte Cristo", "Alexandre Dumas"),
    ("Anne of Green Gables", "L. M. Montgomery"),
    ("The Picture of Dorian Gray", "Oscar Wilde"),
];

/// What [`seed`] wrote, for the log and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedReport {
    /// Library user id of Sam.
    pub user_id: i64,
    /// Books added from Sam's Libro.fm account.
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
/// * `store_plugin_key` - PluginKey of the enrolled [`SAMPLE_STORE`] plugin,
///   which owns Sam's account and books.
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
    store_plugin_key: &str,
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

    store
        .upsert_account(
            store_plugin_key,
            SAMPLE_ACCOUNT,
            "us",
            Some("Sam's Libro.fm"),
            false,
            SAMPLE_STORE,
        )
        .await
        .context("add the sample account")?;
    store
        .link_account(identity.id, store_plugin_key, SAMPLE_ACCOUNT, SAMPLE_STORE)
        .await
        .context("link the sample account")?;
    let bought = Utc::now() - Duration::days(40);
    for (i, sample) in LIBRARY.iter().enumerate() {
        let mut book = NewBook::minimal(sample.product_id, SAMPLE_ACCOUNT, "us", sample.title);
        book.asin = None;
        book.plugin_key = store_plugin_key.to_owned();
        book.source = SAMPLE_STORE.to_owned();
        book.authors = Some(sample.author.to_owned());
        book.series = sample.series.map(|(name, _)| name.to_owned());
        book.series_index = sample.series.map(|(_, index)| index.to_owned());
        book.length_minutes = Some(sample.minutes);
        book.purchased_at = Some(bought + Duration::days(i64::try_from(i)?));
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

    let branch_titles = add_branch(store, cluster_id, user.id).await?;
    Ok(Seeded::Wrote(SeedReport {
        user_id: user.id,
        books: LIBRARY.len(),
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
async fn add_branch(store: &LibraryStore, cluster_id: &str, owner: i64) -> anyhow::Result<u64> {
    let last_seen = (Utc::now() - BRANCH_SILENT_FOR).to_rfc3339();
    let enrolled = (Utc::now() - Duration::days(30)).to_rfc3339();
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
        store: SAMPLE_STORE.into(),
        product_id: sample.product_id.into(),
        metadata: CatalogMetadata {
            title: sample.title.into(),
            authors: Some(sample.author.into()),
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
