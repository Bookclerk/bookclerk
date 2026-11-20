//! Drives the client against a fake Secret Service served over a socket
//! pair, so the D-Bus calls, prompts and error mapping run without a
//! desktop keyring.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, ObjectServer};

use super::{delete_on, get_on, set_on, Bus, SERVICE_PATH};
use crate::{KeychainItemId, KeystoreErrorKind, KEYCHAIN_SERVICE};

/// Where the fake keeps its one collection.
const COLLECTION: &str = "/org/freedesktop/secrets/collection/login";
/// The session object the fake hands out.
const SESSION: &str = "/org/freedesktop/secrets/session/1";

/// One stored item.
#[derive(Clone)]
struct StoredItem {
    /// The item's lookup attributes.
    attributes: HashMap<String, String>,
    /// The item's secret bytes.
    secret: Vec<u8>,
}

/// What the fake service holds and how it behaves.
#[derive(Default)]
struct State {
    /// Items by number; an item's path is `COLLECTION/<number>`.
    items: BTreeMap<u32, StoredItem>,
    /// The next item or prompt number.
    next: u32,
    /// Whether the collection is locked (items are found as locked, and
    /// unlocking needs a prompt).
    locked: bool,
    /// Whether the user dismisses prompts.
    dismiss: bool,
    /// Whether prompts never answer (the service went away mid-prompt).
    silent: bool,
    /// Whether creating the default collection needs a prompt.
    prompt_to_create: bool,
    /// Whether the `default` alias is set (otherwise `ReadAlias` answers `/`).
    alias: bool,
    /// Whether `OpenSession` is refused.
    refuse_session: bool,
    /// Prompts shown so far.
    prompts: u32,
}

/// The shared state, locked without regard to poisoning.
type Shared = Arc<Mutex<State>>;

/// Locks `state`.
fn lock(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An object path from text the fake builds itself.
fn path(text: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(text.to_string()).expect("valid object path")
}

/// The path of item `id`.
fn item_path(id: u32) -> String {
    format!("{COLLECTION}/{id}")
}

/// `org.freedesktop.Secret.Service`.
struct FakeService {
    /// The shared state.
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl FakeService {
    /// Opens a `plain` session.
    async fn open_session(
        &self,
        algorithm: &str,
        _input: OwnedValue,
    ) -> fdo::Result<(OwnedValue, OwnedObjectPath)> {
        if lock(&self.state).refuse_session || algorithm != "plain" {
            return Err(fdo::Error::NotSupported("no session for you".into()));
        }
        let output = OwnedValue::try_from(Value::from("")).expect("owned string");
        Ok((output, path(SESSION)))
    }

    /// Resolves an alias.
    async fn read_alias(&self, name: &str) -> fdo::Result<OwnedObjectPath> {
        let alias = lock(&self.state).alias;
        Ok(path(if alias && name == "default" {
            COLLECTION
        } else {
            "/"
        }))
    }

    /// Finds items whose attributes include `attributes`.
    async fn search_items(
        &self,
        attributes: HashMap<String, String>,
    ) -> fdo::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
        let state = lock(&self.state);
        let found: Vec<OwnedObjectPath> = state
            .items
            .iter()
            .filter(|(_, item)| {
                attributes
                    .iter()
                    .all(|(k, v)| item.attributes.get(k) == Some(v))
            })
            .map(|(id, _)| path(&item_path(*id)))
            .collect();
        Ok(if state.locked {
            (Vec::new(), found)
        } else {
            (found, Vec::new())
        })
    }

    /// Creates the `default` collection (the fake's only one).
    async fn create_collection(
        &self,
        _properties: HashMap<String, OwnedValue>,
        alias: &str,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        if alias != "default" {
            return Err(fdo::Error::InvalidArgs("only the default alias".into()));
        }
        let prompt = {
            let mut state = lock(&self.state);
            if !state.prompt_to_create {
                state.alias = true;
                return Ok((path(COLLECTION), path("/")));
            }
            state.next += 1;
            format!("/org/freedesktop/secrets/prompt/{}", state.next)
        };
        server
            .at(
                prompt.as_str(),
                FakePrompt {
                    state: Arc::clone(&self.state),
                },
            )
            .await?;
        Ok((path("/"), path(&prompt)))
    }

    /// Unlocks objects, through a prompt when the collection is locked.
    async fn unlock(
        &self,
        objects: Vec<OwnedObjectPath>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(Vec<OwnedObjectPath>, OwnedObjectPath)> {
        let prompt = {
            let mut state = lock(&self.state);
            if !state.locked {
                return Ok((objects, path("/")));
            }
            state.next += 1;
            format!("/org/freedesktop/secrets/prompt/{}", state.next)
        };
        server
            .at(
                prompt.as_str(),
                FakePrompt {
                    state: Arc::clone(&self.state),
                },
            )
            .await?;
        Ok((Vec::new(), path(&prompt)))
    }
}

/// `org.freedesktop.Secret.Collection`.
struct FakeCollection {
    /// The shared state.
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.Secret.Collection")]
impl FakeCollection {
    /// Creates an item, or replaces one with the same attributes.
    async fn create_item(
        &self,
        properties: HashMap<String, OwnedValue>,
        secret: (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
        replace: bool,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        let attributes = properties
            .get("org.freedesktop.Secret.Item.Attributes")
            .ok_or_else(|| fdo::Error::InvalidArgs("no attributes".into()))?;
        let attributes: HashMap<String, String> =
            HashMap::try_from(attributes.try_clone().map_err(zbus::Error::from)?)
                .map_err(|err| fdo::Error::InvalidArgs(err.to_string()))?;
        if secret.0.as_str() != SESSION {
            return Err(fdo::Error::InvalidArgs("unknown session".into()));
        }
        let (id, new) = {
            let mut state = lock(&self.state);
            let existing = state
                .items
                .iter()
                .find(|(_, item)| item.attributes == attributes)
                .map(|(id, _)| *id);
            let item = StoredItem {
                attributes,
                secret: secret.2,
            };
            match existing.filter(|_| replace) {
                Some(id) => {
                    state.items.insert(id, item);
                    (id, false)
                }
                None => {
                    state.next += 1;
                    let id = state.next;
                    state.items.insert(id, item);
                    (id, true)
                }
            }
        };
        if new {
            server
                .at(
                    item_path(id),
                    FakeItem {
                        state: Arc::clone(&self.state),
                        id,
                    },
                )
                .await?;
        }
        Ok((path(&item_path(id)), path("/")))
    }
}

/// `org.freedesktop.Secret.Item`.
struct FakeItem {
    /// The shared state.
    state: Shared,
    /// The item's number.
    id: u32,
}

#[zbus::interface(name = "org.freedesktop.Secret.Item")]
impl FakeItem {
    /// The item's secret.
    async fn get_secret(
        &self,
        session: OwnedObjectPath,
    ) -> fdo::Result<(OwnedObjectPath, Vec<u8>, Vec<u8>, String)> {
        let state = lock(&self.state);
        let item = state.items.get(&self.id).ok_or_else(|| {
            fdo::Error::UnknownObject("org.freedesktop.Secret.Error.NoSuchObject".into())
        })?;
        Ok((
            session,
            Vec::new(),
            item.secret.clone(),
            "application/octet-stream".into(),
        ))
    }

    /// Replaces the item's secret.
    async fn set_secret(
        &self,
        secret: (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
    ) -> fdo::Result<()> {
        let mut state = lock(&self.state);
        let item = state
            .items
            .get_mut(&self.id)
            .ok_or_else(|| fdo::Error::UnknownObject("no such item".into()))?;
        item.secret = secret.2;
        Ok(())
    }

    /// Deletes the item.
    async fn delete(&self) -> fdo::Result<OwnedObjectPath> {
        lock(&self.state).items.remove(&self.id);
        Ok(path("/"))
    }
}

/// `org.freedesktop.Secret.Prompt`.
struct FakePrompt {
    /// The shared state.
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.Secret.Prompt")]
impl FakePrompt {
    /// Shows the prompt; the answer comes as `Completed`.
    async fn prompt(
        &self,
        _window_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let (dismissed, silent) = {
            let mut state = lock(&self.state);
            state.prompts += 1;
            if !state.dismiss {
                state.locked = false;
                if state.prompt_to_create {
                    state.alias = true;
                }
            }
            (state.dismiss, state.silent)
        };
        if silent {
            return Ok(());
        }
        Self::completed(&emitter, dismissed, Value::from(""))
            .await
            .map_err(fdo::Error::from)
    }

    /// The prompt's answer.
    #[zbus(signal)]
    async fn completed(
        emitter: &SignalEmitter<'_>,
        dismissed: bool,
        result: Value<'_>,
    ) -> zbus::Result<()>;
}

/// A fake Secret Service; each [`Fake::bus`] call opens one connection to it.
struct Fake {
    /// The shared state.
    state: Shared,
    /// Runs the service side of every connection.
    runtime: tokio::runtime::Runtime,
}

impl Fake {
    /// A fake with the alias set and the collection unlocked.
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let state = Shared::default();
        lock(&state).alias = true;
        Self { state, runtime }
    }

    /// Changes the fake's behavior.
    fn with(&self, change: impl FnOnce(&mut State)) {
        change(&mut lock(&self.state));
    }

    /// A client end served by this fake.
    fn bus(&self) -> Bus {
        let (client, service) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let state = Arc::clone(&self.state);
        // The handshake finishes once the client speaks on its end.
        self.runtime.spawn(async move {
            if let Ok(conn) = serve(service, state).await {
                // Keep the connection until the client hangs up.
                let mut closed = zbus::MessageStream::from(&conn);
                while futures::StreamExt::next(&mut closed).await.is_some() {}
            }
        });
        Bus::Peer(client)
    }
}

/// Serves the fake's objects on one end of a socket pair.
async fn serve(
    stream: std::os::unix::net::UnixStream,
    state: Shared,
) -> zbus::Result<zbus::Connection> {
    stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    let ids: Vec<u32> = lock(&state).items.keys().copied().collect();
    let mut builder = zbus::connection::Builder::unix_stream(stream)
        .server(zbus::Guid::generate())?
        .p2p()
        .serve_at(
            SERVICE_PATH,
            FakeService {
                state: Arc::clone(&state),
            },
        )?
        .serve_at(
            COLLECTION,
            FakeCollection {
                state: Arc::clone(&state),
            },
        )?;
    for id in ids {
        builder = builder.serve_at(
            item_path(id),
            FakeItem {
                state: Arc::clone(&state),
                id,
            },
        )?;
    }
    builder.build().await
}

#[test]
fn an_item_round_trips_and_is_replaced_and_deleted() {
    let fake = Fake::new();
    let item = KeychainItemId::random();

    set_on(fake.bus(), item, b"first").expect("set");
    assert_eq!(get_on(fake.bus(), item).expect("get").as_slice(), b"first");
    set_on(fake.bus(), item, b"second").expect("replace");
    assert_eq!(get_on(fake.bus(), item).expect("get").as_slice(), b"second");
    {
        let state = lock(&fake.state);
        assert_eq!(state.items.len(), 1, "replaced, not added");
        let stored = state.items.values().next().expect("one item");
        assert_eq!(
            stored.attributes.get("service").map(String::as_str),
            Some(KEYCHAIN_SERVICE)
        );
        assert_eq!(stored.attributes.get("username"), Some(&item.to_string()));
    }

    delete_on(fake.bus(), item).expect("delete");
    let missing = get_on(fake.bus(), item).expect_err("gone");
    assert_eq!(missing.kind, KeystoreErrorKind::NotFound);
    delete_on(fake.bus(), item).expect("deleting a missing item is fine");
}

#[test]
fn items_of_other_installs_are_kept_apart() {
    let fake = Fake::new();
    let ours = KeychainItemId::random();
    let theirs = KeychainItemId::random();
    set_on(fake.bus(), ours, b"ours").expect("set");
    set_on(fake.bus(), theirs, b"theirs").expect("set");
    assert_eq!(get_on(fake.bus(), ours).expect("get").as_slice(), b"ours");
    delete_on(fake.bus(), theirs).expect("delete");
    assert_eq!(get_on(fake.bus(), ours).expect("get").as_slice(), b"ours");
}

#[test]
fn a_missing_default_collection_is_created() {
    let fake = Fake::new();
    fake.with(|state| state.alias = false);
    let item = KeychainItemId::random();
    set_on(fake.bus(), item, b"key").expect("set into a new collection");
    assert!(lock(&fake.state).alias);
    assert_eq!(get_on(fake.bus(), item).expect("get").as_slice(), b"key");
}

#[test]
fn creating_the_default_collection_goes_through_its_prompt() {
    let fake = Fake::new();
    fake.with(|state| {
        state.alias = false;
        state.prompt_to_create = true;
    });
    let item = KeychainItemId::random();
    set_on(fake.bus(), item, b"key").expect("set after the prompt");
    assert_eq!(lock(&fake.state).prompts, 1);
    assert_eq!(get_on(fake.bus(), item).expect("get").as_slice(), b"key");

    let refused = Fake::new();
    refused.with(|state| {
        state.alias = false;
        state.prompt_to_create = true;
        state.dismiss = true;
    });
    let err = set_on(refused.bus(), item, b"key").expect_err("dismissed");
    assert_eq!(err.kind, KeystoreErrorKind::AccessDenied);
}

#[test]
fn an_existing_item_is_updated_where_it_is() {
    let fake = Fake::new();
    let item = KeychainItemId::random();
    set_on(fake.bus(), item, b"first").expect("set");
    // The default collection goes away; the item is still found and updated.
    fake.with(|state| state.alias = false);
    set_on(fake.bus(), item, b"second").expect("update");
    let state = lock(&fake.state);
    assert_eq!(state.items.len(), 1);
    assert!(!state.alias, "no new collection was needed");
    assert_eq!(
        state.items.values().next().map(|i| i.secret.as_slice()),
        Some(b"second".as_slice())
    );
}

#[test]
fn a_prompt_that_never_answers_times_out() {
    let fake = Fake::new();
    let item = KeychainItemId::random();
    set_on(fake.bus(), item, b"key").expect("set");
    fake.with(|state| {
        state.locked = true;
        state.silent = true;
    });
    let err = get_on(fake.bus(), item).expect_err("no answer");
    assert_eq!(err.kind, KeystoreErrorKind::Unavailable);
}

#[test]
fn a_locked_collection_is_unlocked_through_the_prompt() {
    let fake = Fake::new();
    let item = KeychainItemId::random();
    set_on(fake.bus(), item, b"key").expect("set");
    fake.with(|state| state.locked = true);
    assert_eq!(get_on(fake.bus(), item).expect("get").as_slice(), b"key");
    assert_eq!(lock(&fake.state).prompts, 1);
}

#[test]
fn a_dismissed_prompt_is_access_denied() {
    let fake = Fake::new();
    let item = KeychainItemId::random();
    set_on(fake.bus(), item, b"key").expect("set");
    fake.with(|state| {
        state.locked = true;
        state.dismiss = true;
    });
    let err = get_on(fake.bus(), item).expect_err("dismissed");
    assert_eq!(err.kind, KeystoreErrorKind::AccessDenied);
    let err = set_on(fake.bus(), item, b"other").expect_err("dismissed");
    assert_eq!(err.kind, KeystoreErrorKind::AccessDenied);
    let err = delete_on(fake.bus(), item).expect_err("dismissed");
    assert_eq!(err.kind, KeystoreErrorKind::AccessDenied);
}

#[test]
fn two_matching_items_are_rejected() {
    let fake = Fake::new();
    let item = KeychainItemId::random();
    let stored = StoredItem {
        attributes: HashMap::from([
            ("service".to_string(), KEYCHAIN_SERVICE.to_string()),
            ("username".to_string(), item.to_string()),
        ]),
        secret: b"key".to_vec(),
    };
    fake.with(|state| {
        state.items.insert(1, stored.clone());
        state.items.insert(2, stored);
        state.next = 2;
    });
    let err = get_on(fake.bus(), item).expect_err("ambiguous");
    assert_eq!(err.kind, KeystoreErrorKind::Rejected);
}

#[test]
fn a_service_without_a_plain_session_is_unavailable() {
    let fake = Fake::new();
    fake.with(|state| state.refuse_session = true);
    let err = get_on(fake.bus(), KeychainItemId::random()).expect_err("no session");
    assert_eq!(err.kind, KeystoreErrorKind::Unavailable);
}

#[test]
fn a_peer_that_hangs_up_is_unavailable() {
    let (client, service) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    drop(service);
    let err = get_on(Bus::Peer(client), KeychainItemId::random()).expect_err("no peer");
    assert_eq!(err.kind, KeystoreErrorKind::Unavailable);
}

#[test]
fn dbus_errors_map_to_kinds() {
    let kind = |text: &str| super::dbus_error(zbus::Error::Failure(text.into())).kind;
    assert_eq!(
        kind("org.freedesktop.Secret.Error.IsLocked"),
        KeystoreErrorKind::AccessDenied
    );
    assert_eq!(
        kind("org.freedesktop.DBus.Error.AccessDenied"),
        KeystoreErrorKind::AccessDenied
    );
    assert_eq!(
        kind("org.freedesktop.Secret.Error.NoSuchObject"),
        KeystoreErrorKind::NotFound
    );
    assert_eq!(kind("something else"), KeystoreErrorKind::Unavailable);
}
