//! A minimal Secret Service client (the freedesktop.org keyring API that
//! GNOME Keyring and KWallet serve on the session D-Bus).
//!
//! It opens a `plain` session rather than the Diffie-Hellman one, so the
//! hosts link no block cipher (the store-free host scan forbids `aes` and
//! `cbc` in their graph). The session bus is the user's own local socket,
//! which only the same user can monitor; that is the same boundary the
//! keychain itself protects against.
//!
//! Each call runs on its own thread with a current-thread Tokio runtime, so
//! callers inside or outside a Tokio runtime can use it without nesting
//! runtimes.

use std::collections::HashMap;

use futures::StreamExt as _;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zeroize::Zeroizing;

use crate::{KeychainItemId, KeystoreError, KeystoreErrorKind, Result, KEYCHAIN_SERVICE};

/// Bus name of the Secret Service.
const DEST: &str = "org.freedesktop.secrets";
/// Object path of the service.
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
/// The service interface.
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
/// The collection interface.
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
/// The item interface.
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
/// The prompt interface.
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";
/// The collection most desktops unlock at login.
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";
/// Content type recorded with every item.
const CONTENT_TYPE: &str = "application/octet-stream";

/// A secret as the API carries it: session, parameters (empty for `plain`),
/// value, content type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// Reads an item's bytes.
pub(crate) fn get(item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>> {
    run(move |client| async move {
        let path = client.find(item).await?.ok_or_else(not_found)?;
        client.unlock(std::slice::from_ref(&path)).await?;
        let proxy = client.proxy(path.as_str(), ITEM_IFACE).await?;
        let (_, _, value, _): Secret = proxy
            .call("GetSecret", &(&client.session,))
            .await
            .map_err(dbus_error)?;
        Ok(Zeroizing::new(value))
    })
}

/// Creates or replaces an item.
pub(crate) fn set(item: KeychainItemId, bytes: &[u8]) -> Result<()> {
    let bytes = Zeroizing::new(bytes.to_vec());
    run(move |client| async move {
        let collection = client.default_collection().await?;
        client.unlock(std::slice::from_ref(&collection)).await?;
        let account = item.to_string();
        let label = format!("Bookclerk library key ({account})");
        let attributes: HashMap<&str, &str> = HashMap::from([
            ("service", KEYCHAIN_SERVICE),
            ("username", account.as_str()),
        ]);
        let properties: HashMap<&str, Value<'_>> = HashMap::from([
            (
                "org.freedesktop.Secret.Item.Label",
                Value::from(label.as_str()),
            ),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(attributes),
            ),
        ]);
        let secret = (
            client.session.clone(),
            Vec::<u8>::new(),
            bytes.to_vec(),
            CONTENT_TYPE.to_string(),
        );
        let proxy = client.proxy(collection.as_str(), COLLECTION_IFACE).await?;
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) = proxy
            .call("CreateItem", &(properties, secret, true))
            .await
            .map_err(dbus_error)?;
        client.complete(prompt).await
    })
}

/// Deletes an item; a missing item is not an error.
pub(crate) fn delete(item: KeychainItemId) -> Result<()> {
    run(move |client| async move {
        let Some(path) = client.find(item).await? else {
            return Ok(());
        };
        client.unlock(std::slice::from_ref(&path)).await?;
        let proxy = client.proxy(path.as_str(), ITEM_IFACE).await?;
        let prompt: OwnedObjectPath = proxy.call("Delete", &()).await.map_err(dbus_error)?;
        client.complete(prompt).await
    })
}

/// A connection to the service with an open `plain` session.
struct Client {
    /// The session bus connection.
    conn: zbus::Connection,
    /// The session object secrets are exchanged under.
    session: OwnedObjectPath,
}

impl Client {
    /// Connects to the session bus and opens a `plain` session.
    async fn connect() -> Result<Self> {
        let conn = zbus::Connection::session().await.map_err(|err| {
            KeystoreError::new(
                KeystoreErrorKind::Unavailable,
                format!("no session D-Bus: {err}"),
            )
        })?;
        let service = proxy(&conn, SERVICE_PATH, SERVICE_IFACE).await?;
        let (_, session): (OwnedValue, OwnedObjectPath) = service
            .call("OpenSession", &("plain", Value::from("")))
            .await
            .map_err(|err| {
                KeystoreError::new(
                    KeystoreErrorKind::Unavailable,
                    format!("no Secret Service on the session bus: {err}"),
                )
            })?;
        Ok(Self { conn, session })
    }

    /// A proxy for `path` and `iface` on the service.
    async fn proxy(&self, path: &str, iface: &'static str) -> Result<zbus::Proxy<'static>> {
        proxy(&self.conn, path, iface).await
    }

    /// The object path of the default collection.
    async fn default_collection(&self) -> Result<OwnedObjectPath> {
        let service = self.proxy(SERVICE_PATH, SERVICE_IFACE).await?;
        let path: OwnedObjectPath = service
            .call("ReadAlias", &("default",))
            .await
            .map_err(dbus_error)?;
        if path.as_str() == "/" {
            // No alias set: most services still answer on the alias path.
            return OwnedObjectPath::try_from(DEFAULT_COLLECTION)
                .map_err(|err| KeystoreError::new(KeystoreErrorKind::Rejected, err.to_string()));
        }
        Ok(path)
    }

    /// The item for `item`, locked or not, if there is one.
    async fn find(&self, item: KeychainItemId) -> Result<Option<OwnedObjectPath>> {
        let account = item.to_string();
        let attributes: HashMap<&str, &str> = HashMap::from([
            ("service", KEYCHAIN_SERVICE),
            ("username", account.as_str()),
        ]);
        let service = self.proxy(SERVICE_PATH, SERVICE_IFACE).await?;
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = service
            .call("SearchItems", &(attributes,))
            .await
            .map_err(dbus_error)?;
        let mut found = unlocked.into_iter().chain(locked);
        let first = found.next();
        if found.next().is_some() {
            return Err(KeystoreError::new(
                KeystoreErrorKind::Rejected,
                "more than one item matches",
            ));
        }
        Ok(first)
    }

    /// Unlocks `objects`, showing the desktop's prompt when one is needed.
    async fn unlock(&self, objects: &[OwnedObjectPath]) -> Result<()> {
        let service = self.proxy(SERVICE_PATH, SERVICE_IFACE).await?;
        let (_, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = service
            .call("Unlock", &(objects,))
            .await
            .map_err(dbus_error)?;
        self.complete(prompt).await
    }

    /// Runs a prompt (`/` means none is needed) and waits for its answer.
    async fn complete(&self, prompt: OwnedObjectPath) -> Result<()> {
        if prompt.as_str() == "/" {
            return Ok(());
        }
        let proxy = self.proxy(prompt.as_str(), PROMPT_IFACE).await?;
        let mut completed = proxy
            .receive_signal("Completed")
            .await
            .map_err(dbus_error)?;
        proxy
            .call::<_, _, ()>("Prompt", &("",))
            .await
            .map_err(dbus_error)?;
        let message = completed.next().await.ok_or_else(|| {
            KeystoreError::new(KeystoreErrorKind::Unavailable, "the prompt went away")
        })?;
        let (dismissed, _): (bool, OwnedValue) = message
            .body()
            .deserialize()
            .map_err(|err| KeystoreError::new(KeystoreErrorKind::Rejected, err.to_string()))?;
        if dismissed {
            return Err(KeystoreError::new(
                KeystoreErrorKind::AccessDenied,
                "the keychain prompt was dismissed",
            ));
        }
        Ok(())
    }
}

/// A proxy for `path` and `iface` on the service.
async fn proxy(
    conn: &zbus::Connection,
    path: &str,
    iface: &'static str,
) -> Result<zbus::Proxy<'static>> {
    let path = ObjectPath::try_from(path.to_string())
        .map_err(|err| KeystoreError::new(KeystoreErrorKind::Rejected, err.to_string()))?;
    zbus::Proxy::new(conn, DEST, path, iface)
        .await
        .map_err(dbus_error)
}

/// The error for a missing item.
fn not_found() -> KeystoreError {
    KeystoreError::new(KeystoreErrorKind::NotFound, "no such item")
}

/// Maps a D-Bus failure. Locked or refused calls are access errors; the
/// rest mean the service is not usable.
fn dbus_error(err: zbus::Error) -> KeystoreError {
    let text = err.to_string();
    let kind = if text.contains("IsLocked") || text.contains("AccessDenied") {
        KeystoreErrorKind::AccessDenied
    } else if text.contains("NoSuchObject") {
        KeystoreErrorKind::NotFound
    } else {
        KeystoreErrorKind::Unavailable
    };
    KeystoreError::new(kind, text)
}

/// Runs `work` against a fresh client on its own thread and runtime.
fn run<T, F, Fut>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Client) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>>,
{
    let handle = std::thread::Builder::new()
        .name("bookclerk-keystore".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| {
                    KeystoreError::new(KeystoreErrorKind::Unavailable, err.to_string())
                })?;
            runtime.block_on(async move { work(Client::connect().await?).await })
        })
        .map_err(|err| KeystoreError::new(KeystoreErrorKind::Unavailable, err.to_string()))?;
    handle.join().unwrap_or_else(|_| {
        Err(KeystoreError::new(
            KeystoreErrorKind::Unavailable,
            "the keychain thread panicked",
        ))
    })
}
