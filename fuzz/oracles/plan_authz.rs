//! Shared oracle for the `plan_authz` fuzz target and the `bookclerk-library`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the scan and acquire planners
//! (`bookclerk_library::control_plane::plan_scan` / `plan_acquire`) acting
//! for a Member: the request body a signed-in Member sends to
//! `POST /api/library/scan` or `POST /api/library/acquire`, or a scan their
//! Host asks for. The input decodes into a small library (store accounts on
//! two plugins with colliding ids and labels, who owns each, library rows,
//! which plugins have an instance) and one request (scan or acquire, an
//! account needle, title ids and an instance pin).
//!
//! The invariant, checked against this file's own model of the library and
//! not the planner's queries: every job planned for the Member names an
//! account, and every store account that job's needle reaches at run time
//! (its id or label, ignoring ASCII case, within the pinned instance's
//! plugin when it is pinned) is linked to the Member. A refusal is
//! `Forbidden` or `Invalid`, never a database error.

use std::collections::BTreeMap;

use bookclerk_library::control_plane::{
    create_plugin_instance, plan_acquire, plan_scan, ConfigActor, ScanRequester,
};
use bookclerk_library::{LibraryError, LibraryStore, NewBook, UserRole};

/// Largest input worth decoding; the scenario reads a few dozen bytes.
pub const MAX_INPUT_BYTES: usize = 256;

/// The two plugins, so an account id can exist on both.
const PLUGINS: [&str; 2] = [
    "path:file:///tmp/store#store",
    "path:file:///tmp/other#other",
];

/// Account ids, with a case variant the needle match folds together.
const IDS: [&str; 4] = ["a", "b", "c", "A"];

/// Account labels, some equal to another account's id.
const LABELS: [Option<&str>; 5] = [None, Some("a"), Some("b"), Some("x"), Some("B")];

/// Account needles a request may name (`" "` is blank, which is invalid).
const NEEDLES: [Option<&str>; 7] = [
    None,
    Some("a"),
    Some("b"),
    Some("c"),
    Some("A"),
    Some("x"),
    Some(" "),
];

/// Product ids and ASINs on library rows, and the title ids a request names.
const TITLES: [&str; 5] = ["t1", "T1", "t2", "t3", " "];

/// Who a store account is linked to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Nobody.
    Nobody,
    /// The Member the request acts for.
    Member,
    /// Another Member.
    Other,
}

/// One store account in the model.
#[derive(Debug, Clone)]
struct Account {
    /// Index into [`PLUGINS`].
    plugin: usize,
    /// Account id.
    id: &'static str,
    /// Optional label.
    label: Option<&'static str>,
    /// Whether scanning is on.
    scan: bool,
    /// Who it is linked to.
    owner: Owner,
}

/// One library row in the model.
#[derive(Debug, Clone)]
struct Book {
    /// Index into [`PLUGINS`].
    plugin: usize,
    /// Account id the row is under (it may name no account).
    account: &'static str,
    /// Product id.
    product_id: &'static str,
    /// ASIN.
    asin: &'static str,
}

/// How the request pins an instance.
#[derive(Debug, Clone, Copy)]
enum Pin {
    /// No pin.
    None,
    /// The default instance of this plugin, when it has one.
    Plugin(usize),
    /// Text that is not an instance id.
    Garbage,
    /// A well-formed id of no instance.
    Unknown,
}

/// The decoded library and request.
#[derive(Debug, Clone)]
struct Scenario {
    /// Store accounts, unique by `(plugin, id)`.
    accounts: Vec<Account>,
    /// Library rows.
    books: Vec<Book>,
    /// Which plugins get a default instance.
    instances: [bool; 2],
    /// True for an acquire, false for a scan.
    acquire: bool,
    /// Account needle.
    needle: Option<&'static str>,
    /// Title ids (acquire only).
    titles: Vec<&'static str>,
    /// Instance pin.
    pin: Pin,
}

/// Reads bytes in order, yielding zero once they run out.
struct Bytes<'a>(std::slice::Iter<'a, u8>);

impl Bytes<'_> {
    /// The next byte, or zero.
    fn next(&mut self) -> usize {
        usize::from(self.0.next().copied().unwrap_or(0))
    }
}

/// Decodes `data` into a scenario.
fn decode(data: &[u8]) -> Scenario {
    let mut b = Bytes(data.iter());
    let mut accounts: Vec<Account> = Vec::new();
    for _ in 0..(b.next() % 6) + 1 {
        let shape = b.next();
        let owner = match b.next() % 3 {
            0 => Owner::Nobody,
            1 => Owner::Member,
            _ => Owner::Other,
        };
        let account = Account {
            plugin: shape & 1,
            id: IDS[(shape >> 1) % IDS.len()],
            label: LABELS[(shape >> 3) % LABELS.len()],
            scan: (shape >> 6) & 1 == 1,
            owner,
        };
        if !accounts
            .iter()
            .any(|a| a.plugin == account.plugin && a.id == account.id)
        {
            accounts.push(account);
        }
    }
    let mut books = Vec::new();
    for _ in 0..b.next() % 5 {
        let shape = b.next();
        books.push(Book {
            plugin: shape & 1,
            account: IDS[(shape >> 1) % IDS.len()],
            product_id: TITLES[(shape >> 3) % 4],
            asin: TITLES[(shape >> 5) % 4],
        });
    }
    let flags = b.next();
    let instances = [flags & 1 == 1, flags & 2 == 2];
    let acquire = flags & 4 == 4;
    let needle = NEEDLES[b.next() % NEEDLES.len()];
    let titles = (0..b.next() % 3)
        .map(|_| TITLES[b.next() % TITLES.len()])
        .collect();
    let pin = match b.next() % 5 {
        0 => Pin::None,
        1 => Pin::Plugin(0),
        2 => Pin::Plugin(1),
        3 => Pin::Garbage,
        _ => Pin::Unknown,
    };
    Scenario {
        accounts,
        books,
        instances,
        acquire,
        needle,
        titles,
        pin,
    }
}

/// Runs one input through the planners and checks the invariant.
///
/// # Returns
///
/// How many jobs were planned (`0` for a refusal or an oversized input),
/// so the replay test can tell that its seeds reach the planners.
///
/// # Panics
///
/// Panics when a planned job reaches an account the Member does not own,
/// names no account, or pins an instance that does not exist, or when the
/// planner fails with anything but `Forbidden` or `Invalid`.
// Harness setup: a library that cannot be built is the harness failing, not
// an input the planners must tolerate.
#[allow(clippy::expect_used)]
pub fn check(data: &[u8]) -> usize {
    if data.len() > MAX_INPUT_BYTES {
        return 0;
    }
    let scenario = decode(data);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(run(&scenario))
}

/// Builds the library, plans the request and checks every job; returns how
/// many jobs were planned.
// Harness setup: a library that cannot be built is the harness failing, not
// an input the planners must tolerate.
#[allow(clippy::expect_used)]
async fn run(s: &Scenario) -> usize {
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .expect("sqlite memory");
    let store = LibraryStore::from_connection(db);
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .expect("member")
        .id;
    let other = store
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .expect("other")
        .id;
    let member_identity = store
        .ensure_local_portal_identity(member, None)
        .await
        .expect("member identity")
        .id;
    let other_identity = store
        .ensure_local_portal_identity(other, None)
        .await
        .expect("other identity")
        .id;
    for a in &s.accounts {
        let plugin = PLUGINS[a.plugin];
        store
            .upsert_account(plugin, a.id, "us", a.label, a.scan, "fuzz")
            .await
            .expect("account");
        let identity = match a.owner {
            Owner::Nobody => continue,
            Owner::Member => member_identity,
            Owner::Other => other_identity,
        };
        store
            .link_account(identity, plugin, a.id, "fuzz")
            .await
            .expect("link");
    }
    for book in &s.books {
        let plugin = PLUGINS[book.plugin];
        store
            .scope(plugin, "fuzz")
            .upsert_book(&NewBook {
                uuid: None,
                product_id: book.product_id.into(),
                plugin_key: plugin.into(),
                source: "fuzz".into(),
                account_id: book.account.into(),
                asin: Some(book.asin.into()),
                isbn: None,
                marketplace: "us".into(),
                title: format!("Title {}", book.product_id),
                authors: None,
                narrators: None,
                series: None,
                series_index: None,
                series_asin: None,
                purchased_at: None,
                publisher: None,
                length_minutes: None,
                is_abridged: false,
                content_kind: "book".into(),
                categories: None,
                subtitle: None,
                published_at: None,
            })
            .await
            .expect("book");
    }
    let operator = ConfigActor::Operator { id: "fuzz".into() };
    // Instance id → index into `PLUGINS`.
    let mut instances: BTreeMap<String, usize> = BTreeMap::new();
    for (plugin, wanted) in s.instances.iter().enumerate() {
        if *wanted {
            let instance = create_plugin_instance(&store, &operator, PLUGINS[plugin], "")
                .await
                .expect("instance");
            instances.insert(instance.id.to_string(), plugin);
        }
    }
    let pin: Option<String> = match s.pin {
        Pin::None => None,
        Pin::Plugin(plugin) => instances
            .iter()
            .find(|(_, p)| **p == plugin)
            .map(|(id, _)| id.clone()),
        Pin::Garbage => Some("not-an-instance".into()),
        Pin::Unknown => Some("00000000-0000-4000-8000-000000000000".into()),
    };
    let requester = ScanRequester::User(member);
    let plan = if s.acquire {
        let titles: Vec<String> = s.titles.iter().map(|t| (*t).to_string()).collect();
        plan_acquire(&store, requester, s.needle, &titles, pin.as_deref()).await
    } else {
        plan_scan(&store, requester, s.needle, pin.as_deref()).await
    };
    let jobs = match plan {
        Ok(jobs) => jobs,
        Err(LibraryError::Forbidden(_) | LibraryError::Invalid(_)) => return 0,
        Err(err) => panic!("planner failed with {err} for {s:?}"),
    };
    for job in &jobs {
        let needle = job
            .account
            .as_deref()
            .unwrap_or_else(|| panic!("a Member's job names no account: {job:?} for {s:?}"));
        let plugin = job.plugin_instance_id.as_deref().map(|id| {
            *instances
                .get(id)
                .unwrap_or_else(|| panic!("job pins unknown instance {id}: {s:?}"))
        });
        let touched: Vec<&Account> = s
            .accounts
            .iter()
            .filter(|a| plugin.is_none_or(|p| p == a.plugin))
            .filter(|a| {
                a.id.eq_ignore_ascii_case(needle)
                    || a.label.is_some_and(|l| l.eq_ignore_ascii_case(needle))
            })
            .collect();
        assert!(
            !touched.is_empty(),
            "a Member's job reaches no account: {job:?} for {s:?}"
        );
        for row in touched {
            assert_eq!(
                row.owner,
                Owner::Member,
                "a Member's job reaches plugin {} id {:?} owned by {:?}: {job:?} for {s:?}",
                row.plugin,
                row.id,
                row.owner
            );
        }
    }
    jobs.len()
}
