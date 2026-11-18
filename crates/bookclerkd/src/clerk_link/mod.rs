//! On a clerk: the libraries this install holds a library card for, and
//! the books it reads from them for its person.
//!
//! A clerk signs in to a library as a person ([`api`]: Settings → Library
//! → Library cards) and is not a node of it: it claims no work, runs none
//! of that library's plugins, and holds a card instead of node keys
//! (`bookclerk_library::control_plane::record_clerk_link`).
//!
//! For every active clerk link, one [`ClerkReader`] keeps the library's
//! clerk socket (`GET /api/clerk/v1/connect`) open with the card:
//!
//! - on each connection, on every `changed` frame naming the `library`
//!   topic, and every [`READ_POLL`], it reads every page of the person's
//!   books (`books` → `books.page`) into the window books rows
//!   (`control_plane::window_books`), keyed by the library's cluster id,
//!   so "Your books in other libraries" merges them with those of window
//!   links. Once the last page is in, the books the read no longer named
//!   are dropped; a read cut short drops nothing.
//! - while the socket is welcomed, this install's acquires may ask the
//!   reader for the library's copy of a title ([`copies`]): it sends
//!   `copy.grant` and hands the acquire the `copy.granted` answer, or
//!   nothing when the library answers `error`. The library answers frames
//!   in the order they were asked, so each answer goes to the oldest
//!   frame still waiting for one.
//! - a library that ends the card (close [`CLOSE_UNAUTHORIZED`], or 401 on
//!   the upgrade) signs the link out here: its card and books are dropped
//!   and the link stays listed as signed out, to sign in again.
//! - a lost connection is dialed again with a growing delay
//!   ([`RECONNECT_MIN`] to [`RECONNECT_MAX`]).
//!
//! [`spawn_clerk_links`] starts one reader per active link of the live
//! library, starts readers for links signed in later
//! ([`notify_clerk_links_changed`]), stops those signed out, and restarts
//! the set on the new store after a database swap.

pub mod api;
pub(crate) mod copies;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bookclerk_library::control_plane::{
    discard_window_read, finish_window_read, list_clerk_links, load_clerk_card, load_clerk_link,
    record_window_page, sign_out_clerk_link, window_read_stamp, LiveTopic,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::{DialError, OwnerEndpoint, OwnerSocket, SocketMessage as Message};
use bookclerk_node_proto::{
    parse_library_frame, ClerkFrame, ClerkTopic, LibraryFrame, WindowCursor, MAX_FRAME_BYTES,
};
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use self::copies::GrantAsk;
use crate::api::AppState;
use crate::live::CLOSE_UNAUTHORIZED;

/// Between reads while connected, besides the library's `changed` frames.
pub const READ_POLL: Duration = Duration::from_secs(1800);

/// First delay before a lost clerk socket is dialed again.
pub const RECONNECT_MIN: Duration = Duration::from_secs(5);

/// Longest delay between attempts to dial a clerk socket.
pub const RECONNECT_MAX: Duration = Duration::from_secs(300);

/// How long the library may take to answer one `books` frame.
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long after a library answers `error` to a `books` frame the read
/// is tried again.
const ERROR_RETRY: Duration = Duration::from_secs(60);

/// How often the supervisor reads the links again without being told.
const SUPERVISE_POLL: Duration = Duration::from_secs(60);

/// Most pages one read takes, so a read always ends (as the window
/// reader's limit: the library's cursor must move forward on every page).
const MAX_PAGES_PER_READ: usize = 1_000_000;

/// Bumped when a clerk link is signed in or out here; wakes the
/// supervisor.
static CLERK_LINKS_CHANGED: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// Tells the supervisor the clerk links changed (a sign-in or a sign-out
/// here), so a reader starts or stops at once.
pub fn notify_clerk_links_changed() {
    CLERK_LINKS_CHANGED.send_modify(|generation| *generation = generation.wrapping_add(1));
}

/// Starts the clerk-link supervisor.
pub fn spawn_clerk_links(state: Arc<AppState>) {
    tokio::spawn(supervise_clerk_links(state));
}

/// One running reader.
struct Running {
    /// Tells the reader to stop.
    stop: watch::Sender<bool>,
    /// The reader's task.
    task: JoinHandle<()>,
}

/// Stops `run` and waits a moment for it, aborting it if it does not exit.
async fn stop_reader(run: Running) {
    let _ = run.stop.send(true);
    let mut task = run.task;
    if tokio::time::timeout(Duration::from_secs(5), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// Runs one reader per active clerk link of the live library until the
/// library is swapped, then stops them and starts the new store's set.
async fn supervise_clerk_links(state: Arc<AppState>) {
    let mut swaps = state.library_swaps.subscribe();
    let mut changes = CLERK_LINKS_CHANGED.subscribe();
    loop {
        swaps.borrow_and_update();
        let library = state.library_snapshot().await;
        let mut running: BTreeMap<String, Running> = BTreeMap::new();
        let swapped = loop {
            changes.borrow_and_update();
            match list_clerk_links(&library).await {
                Ok(links) => {
                    let active: Vec<String> = links
                        .into_iter()
                        .filter(|link| link.is_active())
                        .map(|link| link.library_cluster_id)
                        .collect();
                    let gone: Vec<String> = running
                        .keys()
                        .filter(|id| !active.contains(id))
                        .cloned()
                        .collect();
                    for id in gone {
                        if let Some(run) = running.remove(&id) {
                            stop_reader(run).await;
                        }
                    }
                    for id in active {
                        let finished = running.get(&id).is_some_and(|run| run.task.is_finished());
                        if running.contains_key(&id) && !finished {
                            continue;
                        }
                        let (stop, shutdown) = watch::channel(false);
                        let reader = ClerkReader {
                            library: library.clone(),
                            cluster_id: id.clone(),
                        };
                        let task = tokio::spawn(reader.run(shutdown));
                        running.insert(id, Running { stop, task });
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "could not read this install's library cards; retrying");
                }
            }
            tokio::select! {
                changed = swaps.changed() => break changed.is_ok(),
                Ok(()) = changes.changed() => {}
                () = tokio::time::sleep(SUPERVISE_POLL) => {}
            }
        };
        for run in std::mem::take(&mut running).into_values() {
            stop_reader(run).await;
        }
        if !swapped {
            return;
        }
    }
}

/// How one connection to the library ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// The reader was told to stop.
    Shutdown,
    /// The link is gone or signed out here; nothing more to dial.
    Inactive,
    /// The library ended the card; the link is now signed out.
    SignedOut,
    /// The connection was lost after it was welcomed; dial again soon.
    Lost,
}

/// Reads one library's books for this install's person over its clerk
/// socket.
pub struct ClerkReader {
    /// This install's library.
    pub library: LibraryStore,
    /// The library the card is for.
    pub cluster_id: String,
}

/// A frame sent to the library that waits for its answer.
enum Asked {
    /// A `books` frame of the read in progress.
    Books,
    /// A `copy.grant` an acquire waits on.
    Grant(GrantAsk),
}

/// One read in progress over the socket.
struct Read {
    /// The read's stamp ([`window_read_stamp`]).
    seen_at: String,
    /// The cursor the outstanding `books` frame asked with.
    after: WindowCursor,
    /// Pages taken so far.
    pages: usize,
    /// Editions stored so far.
    stored: usize,
    /// When the outstanding page must have arrived.
    deadline: tokio::time::Instant,
}

impl ClerkReader {
    /// Dials the library until the link is signed out, gone, or `shutdown`
    /// turns true.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut delay = RECONNECT_MIN;
        loop {
            if *shutdown.borrow() {
                return;
            }
            match self.session(&mut shutdown).await {
                Ok(SessionEnd::Shutdown | SessionEnd::Inactive) => return,
                Ok(SessionEnd::SignedOut) => {
                    tracing::info!(
                        cluster_id = %self.cluster_id,
                        "the library ended this install's library card; signed out"
                    );
                    return;
                }
                Ok(SessionEnd::Lost) => delay = RECONNECT_MIN,
                Err(err) => tracing::warn!(
                    cluster_id = %self.cluster_id,
                    error = %err,
                    "could not read the library's books with the library card; retrying later"
                ),
            }
            tokio::select! {
                result = shutdown.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(RECONNECT_MAX);
        }
    }

    /// Signs the link out here and tells the web app.
    async fn sign_out(&self) -> anyhow::Result<SessionEnd> {
        sign_out_clerk_link(&self.library, &self.cluster_id).await?;
        crate::live::poke(LiveTopic::Clusters);
        Ok(SessionEnd::SignedOut)
    }

    /// Dials the clerk socket once and serves it until it ends.
    ///
    /// # Errors
    ///
    /// Returns an error when the card cannot be read, the library cannot
    /// be reached or refuses (other than ending the card), it sends a frame
    /// the clerk cannot read, or a local write fails.
    pub async fn session(
        &self,
        shutdown: &mut watch::Receiver<bool>,
    ) -> anyhow::Result<SessionEnd> {
        let Some(link) = load_clerk_link(&self.library, &self.cluster_id).await? else {
            return Ok(SessionEnd::Inactive);
        };
        if !link.is_active() {
            return Ok(SessionEnd::Inactive);
        }
        let card = load_clerk_card(&self.library, &link).await?;
        bookclerk_config::register_secret(card.expose());
        let endpoint = OwnerEndpoint::new(&link.origin, link.spki_pin.as_deref())?;
        let socket = tokio::select! {
            socket = endpoint.connect_clerk_socket(card.expose()) => socket,
            _ = shutdown.changed() => return Ok(SessionEnd::Shutdown),
        };
        drop(card);
        let socket = match socket {
            Ok(socket) => socket,
            Err(DialError::Refused(401)) => return self.sign_out().await,
            Err(err) => return Err(err.into()),
        };
        self.serve(socket, &link.created_at, shutdown).await
    }

    /// Serves a welcomed socket: reads on connect, on `changed` naming the
    /// library topic, and every [`READ_POLL`]; sends the `copy.grant`
    /// frames this install's acquires ask for meanwhile.
    ///
    /// `signed_in_at` is the sign-in time of the card the socket was dialed
    /// with: a grant is asked only while that card is still the link's
    /// active one, so a session left over from a card signed out (or
    /// replaced by a new sign-in) never answers for the current card.
    async fn serve(
        &self,
        mut socket: OwnerSocket,
        signed_in_at: &str,
        shutdown: &mut watch::Receiver<bool>,
    ) -> anyhow::Result<SessionEnd> {
        let (asks_tx, mut asks) = mpsc::channel(copies::ASK_QUEUE);
        let _registered = copies::register(&self.cluster_id, asks_tx);
        // Frames sent and not yet answered, oldest first.
        let mut waiting: VecDeque<Asked> = VecDeque::new();
        let mut read: Option<Read> = None;
        // Read at once on connecting.
        let mut want_read = true;
        let mut next_read = tokio::time::Instant::now() + READ_POLL;
        loop {
            if read.is_none() && want_read {
                want_read = false;
                next_read = tokio::time::Instant::now() + READ_POLL;
                read = Some(
                    self.start_read(&mut socket, window_read_stamp(), WindowCursor::start())
                        .await?,
                );
                waiting.push_back(Asked::Books);
            }
            let deadline = read.as_ref().map_or(next_read, |read| read.deadline);
            let message = tokio::select! {
                _ = shutdown.changed() => {
                    let _ = socket.close(None).await;
                    return Ok(SessionEnd::Shutdown);
                }
                () = tokio::time::sleep_until(deadline) => {
                    if read.take().is_some() {
                        anyhow::bail!("the library did not answer a page of books in time");
                    }
                    want_read = true;
                    continue;
                }
                Some(ask) = asks.recv() => {
                    if !self.same_card(signed_in_at).await? {
                        // Dropping the ask answers it with no copy; dial
                        // again with the link's current card, if any.
                        let _ = socket.close(None).await;
                        return Ok(SessionEnd::Lost);
                    }
                    send(
                        &mut socket,
                        ClerkFrame::CopyGrant {
                            store: ask.store.clone(),
                            marketplace: ask.marketplace.clone(),
                            product_id: ask.product_id.clone(),
                        },
                    )
                    .await?;
                    waiting.push_back(Asked::Grant(ask));
                    continue;
                }
                message = socket.next() => message,
            };
            let text = match message {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Close(frame))) => {
                    if frame.is_some_and(|frame| u16::from(frame.code) == CLOSE_UNAUTHORIZED) {
                        return self.sign_out().await;
                    }
                    return Ok(SessionEnd::Lost);
                }
                // Pings are answered by the socket itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Binary(_))) => {
                    anyhow::bail!("the library sent a binary frame");
                }
                Some(Err(err)) => {
                    return Err(err.into());
                }
                None => {
                    return Ok(SessionEnd::Lost);
                }
            };
            if text.len() > MAX_FRAME_BYTES {
                anyhow::bail!("the library sent a frame over the limit");
            }
            let Some(frame) = parse_library_frame(text.as_bytes()) else {
                anyhow::bail!("the library sent a frame this clerk cannot read");
            };
            match frame {
                LibraryFrame::Hello { .. } => {}
                LibraryFrame::Changed { topics } => {
                    if topics.contains(&ClerkTopic::Library) {
                        want_read = true;
                    }
                }
                LibraryFrame::BooksPage { after, works, next } => {
                    let (Some(Asked::Books), Some(mut current)) =
                        (waiting.pop_front(), read.take())
                    else {
                        anyhow::bail!("the library sent a page nobody asked for");
                    };
                    if after != current.after {
                        anyhow::bail!("the library answered for another page");
                    }
                    current.stored += record_window_page(
                        &self.library,
                        &self.cluster_id,
                        &current.seen_at,
                        &works,
                    )
                    .await?;
                    // Signed out while the page was on its way: what the
                    // read wrote goes with the card.
                    if !self.still_active().await? {
                        self.discard(&current).await;
                        return Ok(SessionEnd::Inactive);
                    }
                    match next {
                        // The frame's parser refuses a `next` that does
                        // not sort after `after`, so every read ends.
                        Some(next) => {
                            if current.pages >= MAX_PAGES_PER_READ {
                                anyhow::bail!("the library kept paging");
                            }
                            current.after = next.clone();
                            current.pages += 1;
                            current.deadline = tokio::time::Instant::now() + PAGE_TIMEOUT;
                            send(&mut socket, ClerkFrame::Books { after: next }).await?;
                            waiting.push_back(Asked::Books);
                            read = Some(current);
                        }
                        None => self.finish(current).await?,
                    }
                }
                LibraryFrame::CopyGranted(granted) => {
                    let Some(Asked::Grant(ask)) = waiting.pop_front() else {
                        anyhow::bail!("the library sent a copy nobody asked for");
                    };
                    if !ask.answered_by(&granted) {
                        anyhow::bail!("the library granted a copy of another edition");
                    }
                    // The acquire may have stopped waiting; then the copy
                    // goes unused.
                    let _ = ask.reply.send(Some(*granted));
                }
                LibraryFrame::Error { code } => match waiting.pop_front() {
                    Some(Asked::Books) => {
                        read = None;
                        tracing::debug!(
                            cluster_id = %self.cluster_id,
                            ?code,
                            "the library could not answer a page of books; reading again later"
                        );
                        next_read = tokio::time::Instant::now() + ERROR_RETRY;
                    }
                    Some(Asked::Grant(ask)) => {
                        tracing::debug!(
                            cluster_id = %self.cluster_id,
                            ?code,
                            "the library granted no copy"
                        );
                        let _ = ask.reply.send(None);
                    }
                    None => anyhow::bail!("the library answered a frame nobody sent"),
                },
            }
        }
    }

    /// Asks for the first page of a read.
    async fn start_read(
        &self,
        socket: &mut OwnerSocket,
        seen_at: String,
        after: WindowCursor,
    ) -> anyhow::Result<Read> {
        send(
            socket,
            ClerkFrame::Books {
                after: after.clone(),
            },
        )
        .await?;
        Ok(Read {
            seen_at,
            after,
            pages: 1,
            stored: 0,
            deadline: tokio::time::Instant::now() + PAGE_TIMEOUT,
        })
    }

    /// Ends a complete read: drops the rows it no longer named.
    async fn finish(&self, read: Read) -> anyhow::Result<()> {
        let dropped = finish_window_read(&self.library, &self.cluster_id, &read.seen_at).await?;
        if !self.still_active().await? {
            self.discard(&read).await;
            return Ok(());
        }
        tracing::debug!(
            cluster_id = %self.cluster_id,
            stored = read.stored,
            dropped,
            "read the books the library holds for this install's person"
        );
        if read.stored > 0 || dropped > 0 {
            crate::live::poke(LiveTopic::Clusters);
        }
        Ok(())
    }

    /// Drops what `read` wrote, for a link signed out while it ran (the
    /// sign-out's own clear may have run before the read's last write). A
    /// read cut short for any other reason is left as it is: its rows
    /// (re-stamped or new) are kept until the next complete read, which
    /// drops what it no longer names. A failure is only logged.
    async fn discard(&self, read: &Read) {
        if let Err(err) = discard_window_read(&self.library, &self.cluster_id, &read.seen_at).await
        {
            tracing::warn!(
                cluster_id = %self.cluster_id,
                error = %err,
                "could not drop an unfinished read of the library's books"
            );
        }
    }

    /// True while the link is active here with the card signed in at
    /// `signed_in_at`.
    async fn same_card(&self, signed_in_at: &str) -> anyhow::Result<bool> {
        Ok(load_clerk_link(&self.library, &self.cluster_id)
            .await?
            .is_some_and(|link| link.is_active() && link.created_at == signed_in_at))
    }

    /// True while the link is still active here.
    async fn still_active(&self) -> anyhow::Result<bool> {
        Ok(load_clerk_link(&self.library, &self.cluster_id)
            .await?
            .is_some_and(|link| link.is_active()))
    }
}

/// Sends one clerk frame.
async fn send(socket: &mut OwnerSocket, frame: ClerkFrame) -> anyhow::Result<()> {
    tokio::time::timeout(
        Duration::from_secs(10),
        socket.send(Message::Text(frame.encode().into())),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the library did not take a frame in time"))??;
    Ok(())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
