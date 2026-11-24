//! OS keychain access for the library's data-encryption key.
//!
//! Bookclerk can keep the library's data-encryption key (DEK) in the
//! operating system's credential store instead of in `master.key`:
//!
//! | Platform | Store |
//! | --- | --- |
//! | macOS | Keychain Services (the login keychain) |
//! | Windows | Credential Manager (DPAPI, per user) |
//! | Linux and other Unix | Secret Service over the session D-Bus (GNOME Keyring, KWallet), in a `plain` session so the hosts link no block cipher |
//!
//! This crate is deliberately small. It names one keychain item per install
//! ([`KeychainItemId`], a random UUID minted with the key), reads, writes and
//! deletes that item's bytes through a [`Vault`], and encodes the pointer
//! record ([`KeychainPointer`]) that `master.key` holds on a keychain
//! install. What the item's bytes mean (`BCK1` or `BCK2`, the fingerprint
//! check) stays in `bookclerk-library`'s `master_key` module.
//!
//! The same stores also keep a hosted database's API token, already sealed
//! with the DEK, as a second [`ItemKind`] with its own service and
//! credential names.
//!
//! [`OsKeychain`] is the real store. [`MemoryVault`] keeps items in memory,
//! for tests and for hosts that must never touch the keychain.
//!
//! Errors never carry item bytes: the keychain libraries' own error values can
//! hold the secret they failed to decode, so [`KeystoreError`] keeps only a
//! category and a safe message.
//!
//! See `docs/adr/master-key-store.md`.

use std::collections::HashMap;
use std::sync::Mutex;

use zeroize::Zeroizing;

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
mod secret_service;
mod systemd_creds;

pub use systemd_creds::{SystemdCredsVault, CREDENTIAL_NAME, DATABASE_CREDENTIAL_NAME};

/// Keychain service name every Bookclerk master-key item is stored under.
///
/// The item's account (user) name is its [`KeychainItemId`], so two installs
/// for one OS user (a development checkout beside an installed app) keep
/// separate items.
pub const KEYCHAIN_SERVICE: &str = "app.bookclerk.master-key";

/// Keychain service name a database credential item is stored under (the
/// DEK-sealed API token of a hosted database such as Cloudflare D1).
pub const DATABASE_CREDENTIAL_SERVICE: &str = "app.bookclerk.database-credential";

/// What an item holds, which decides the names it is stored under.
///
/// Each kind has its own keychain service, `systemd-creds` credential name
/// and sealed-file prefix, so an item of one kind can never be read as the
/// other (a unit-provided master key is never returned for a database
/// credential, and a sealed file can't be renamed into the other's place).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ItemKind {
    /// The library's data-encryption key envelope (`BCK1`/`BCK2`).
    #[default]
    MasterKey,
    /// A hosted database's API credential, already sealed with the DEK.
    DatabaseCredential,
}

impl ItemKind {
    /// The keychain service items of this kind are stored under.
    #[must_use]
    pub fn service(self) -> &'static str {
        match self {
            Self::MasterKey => KEYCHAIN_SERVICE,
            Self::DatabaseCredential => DATABASE_CREDENTIAL_SERVICE,
        }
    }

    /// The `systemd-creds` credential name items of this kind are sealed
    /// under, and the name a service unit hands one in as.
    #[must_use]
    pub fn credential_name(self) -> &'static str {
        match self {
            Self::MasterKey => CREDENTIAL_NAME,
            Self::DatabaseCredential => DATABASE_CREDENTIAL_NAME,
        }
    }

    /// The file-name prefix of a sealed `systemd-creds` item:
    /// `<prefix>-<item>.cred`.
    #[must_use]
    pub fn file_prefix(self) -> &'static str {
        match self {
            Self::MasterKey => "master-key",
            Self::DatabaseCredential => "database-credential",
        }
    }

    /// The human-readable label a keychain shows for an item of this kind.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::MasterKey => "Bookclerk library key",
            Self::DatabaseCredential => "Bookclerk database credential",
        }
    }
}

/// Four-byte header of a `master.key` pointer record for an OS keychain
/// item (`BCK3`).
pub const POINTER_MAGIC: &[u8; 4] = b"BCK3";

/// Four-byte header of a `master.key` pointer record for a key sealed with
/// `systemd-creds` (`BCK4`): the same layout as [`POINTER_MAGIC`]'s.
pub const CREDS_POINTER_MAGIC: &[u8; 4] = b"BCK4";

/// Length in bytes of a key fingerprint (SHA-256 of the DEK).
pub const FINGERPRINT_LEN: usize = 32;

/// Length in bytes of an encoded [`KeychainPointer`]: header, 16-byte item
/// id, 32-byte fingerprint.
pub const POINTER_LEN: usize = 4 + 16 + FINGERPRINT_LEN;

/// Largest item, in bytes, a [`Vault`] accepts.
///
/// A `BCK2` envelope is 108 bytes; Windows Credential Manager caps a blob at
/// 2,560 bytes. Anything larger is refused before it reaches the store.
pub const MAX_ITEM_LEN: usize = 2048;

/// Why a keychain operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeystoreErrorKind {
    /// No credential store is reachable on this machine or session: no
    /// Secret Service on the session bus, no login keychain, no D-Bus.
    Unavailable,
    /// The store is there but refused access (locked collection, the user
    /// denied the prompt, or the app's signature changed).
    AccessDenied,
    /// The named item does not exist.
    NotFound,
    /// The item exists but its contents are not what Bookclerk wrote.
    Corrupt,
    /// The store rejected the request (too long, bad attributes, ambiguous).
    Rejected,
}

/// Which store a [`KeystoreError`] came from, so messages name the store
/// the operator configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorStore {
    /// The OS keychain (also the label for the in-memory test store).
    #[default]
    OsKeychain,
    /// A `systemd-creds` credential ([`SystemdCredsVault`]).
    SystemdCreds,
}

impl ErrorStore {
    /// How the store is named at the start of an error message.
    fn label(self) -> &'static str {
        match self {
            Self::OsKeychain => "OS keychain",
            Self::SystemdCreds => "systemd-creds",
        }
    }
}

/// A key store failure, without any item bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{} {}: {message}", .store.label(), kind_label(.kind))]
pub struct KeystoreError {
    /// What went wrong, for callers that branch on it.
    pub kind: KeystoreErrorKind,
    /// The store that failed.
    pub store: ErrorStore,
    /// A human-readable explanation that never contains item bytes.
    pub message: String,
}

impl KeystoreError {
    /// Builds an OS keychain error of `kind` with `message`; see
    /// [`KeystoreError::in_store`] for another store.
    ///
    /// # Arguments
    ///
    /// * `kind` - the failure category.
    /// * `message` - a safe, secret-free explanation.
    #[must_use]
    pub fn new(kind: KeystoreErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            store: ErrorStore::OsKeychain,
            message: message.into(),
        }
    }

    /// The same error, naming `store` as the one that failed.
    ///
    /// # Arguments
    ///
    /// * `store` - the store the error came from.
    #[must_use]
    pub fn in_store(self, store: ErrorStore) -> Self {
        Self { store, ..self }
    }
}

/// Short label for an error kind, used in [`KeystoreError`]'s message.
fn kind_label(kind: &KeystoreErrorKind) -> &'static str {
    match kind {
        KeystoreErrorKind::Unavailable => "unavailable",
        KeystoreErrorKind::AccessDenied => "access denied",
        KeystoreErrorKind::NotFound => "item not found",
        KeystoreErrorKind::Corrupt => "item unreadable",
        KeystoreErrorKind::Rejected => "request rejected",
    }
}

/// Result alias for keychain operations.
pub type Result<T> = std::result::Result<T, KeystoreError>;

/// Identifier of one install's master-key item: the item's account name
/// under [`KEYCHAIN_SERVICE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeychainItemId(uuid::Uuid);

impl KeychainItemId {
    /// Mints a fresh random item id (UUID v4).
    #[must_use]
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// The id's 16 raw bytes, as the pointer record stores them.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Rebuilds an id from the 16 bytes a pointer record holds.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(uuid::Uuid::from_bytes(bytes))
    }
}

impl std::fmt::Display for KeychainItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// Which store a pointer record names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerStore {
    /// The OS keychain ([`OsKeychain`]); the record starts `BCK3`.
    Keychain,
    /// A credential sealed with `systemd-creds` ([`SystemdCredsVault`]); the
    /// record starts `BCK4`.
    SystemdCreds,
}

impl PointerStore {
    /// The record header for this store.
    #[must_use]
    pub fn magic(self) -> &'static [u8; 4] {
        match self {
            Self::Keychain => POINTER_MAGIC,
            Self::SystemdCreds => CREDS_POINTER_MAGIC,
        }
    }

    /// The store whose records start with `raw`'s first four bytes, if any.
    ///
    /// # Arguments
    ///
    /// * `raw` - the start of `master.key`.
    #[must_use]
    pub fn for_header(raw: &[u8]) -> Option<Self> {
        match raw.get(..4) {
            Some(head) if head == POINTER_MAGIC.as_slice() => Some(Self::Keychain),
            Some(head) if head == CREDS_POINTER_MAGIC.as_slice() => Some(Self::SystemdCreds),
            _ => None,
        }
    }
}

/// What `master.key` holds on a keychain or `systemd-creds` install: which
/// store and item carry the key, and the fingerprint the key inside must
/// match. No key material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeychainPointer {
    /// The store that holds the item.
    pub store: PointerStore,
    /// The item that holds the key envelope.
    pub item: KeychainItemId,
    /// SHA-256 of the unwrapped DEK, the same value `library_identity`
    /// stores. A key read from the item must hash to this.
    pub fingerprint: [u8; FINGERPRINT_LEN],
}

/// Why a byte string is not a pointer record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PointerError {
    /// The bytes start with neither [`POINTER_MAGIC`] nor
    /// [`CREDS_POINTER_MAGIC`].
    #[error("not a BCK3 or BCK4 pointer record")]
    NotAPointer,
    /// The header is right but the record is not exactly [`POINTER_LEN`] bytes.
    #[error("pointer record is {0} bytes, expected {POINTER_LEN}")]
    BadLength(usize),
}

impl KeychainPointer {
    /// Encodes the record: the store's header (`BCK3` or `BCK4`), the item
    /// id, the fingerprint.
    #[must_use]
    pub fn encode(&self) -> [u8; POINTER_LEN] {
        let mut out = [0u8; POINTER_LEN];
        out[..4].copy_from_slice(self.store.magic());
        out[4..20].copy_from_slice(self.item.as_bytes());
        out[20..].copy_from_slice(&self.fingerprint);
        out
    }

    /// Parses a record [`Self::encode`] wrote.
    ///
    /// # Arguments
    ///
    /// * `raw` - the whole contents of `master.key`.
    ///
    /// # Errors
    ///
    /// [`PointerError::NotAPointer`] when the header is neither `BCK3` nor
    /// `BCK4`, and [`PointerError::BadLength`] when the length is not
    /// exactly [`POINTER_LEN`].
    pub fn parse(raw: &[u8]) -> std::result::Result<Self, PointerError> {
        let store = PointerStore::for_header(raw).ok_or(PointerError::NotAPointer)?;
        let item: [u8; 16] = raw
            .get(4..20)
            .and_then(|s| s.try_into().ok())
            .ok_or(PointerError::BadLength(raw.len()))?;
        let fingerprint: [u8; FINGERPRINT_LEN] = raw
            .get(20..)
            .and_then(|s| s.try_into().ok())
            .ok_or(PointerError::BadLength(raw.len()))?;
        Ok(Self {
            store,
            item: KeychainItemId::from_bytes(item),
            fingerprint,
        })
    }
}

/// A store of master-key items: the OS keychain, or memory in tests.
pub trait Vault: Send + Sync {
    /// Reads an item's bytes.
    ///
    /// # Errors
    ///
    /// [`KeystoreErrorKind::NotFound`] when the item is missing, and the
    /// store's other failures.
    fn get(&self, item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>>;

    /// Creates or replaces an item.
    ///
    /// # Errors
    ///
    /// [`KeystoreErrorKind::Rejected`] when `bytes` is longer than
    /// [`MAX_ITEM_LEN`], and the store's failures.
    fn set(&self, item: KeychainItemId, bytes: &[u8]) -> Result<()>;

    /// Deletes an item. Deleting a missing item succeeds.
    ///
    /// # Errors
    ///
    /// The store's failures other than a missing item.
    fn delete(&self, item: KeychainItemId) -> Result<()>;
}

/// Refuses an item larger than [`MAX_ITEM_LEN`].
fn check_len(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_ITEM_LEN {
        return Err(KeystoreError::new(
            KeystoreErrorKind::Rejected,
            format!("item is {} bytes, the limit is {MAX_ITEM_LEN}", bytes.len()),
        ));
    }
    Ok(())
}

/// The operating system's credential store.
///
/// On macOS and Windows the native store (through `keyring-core`) is opened
/// on first use and reused. On Linux and other Unix systems each call talks
/// to the Secret Service on the session D-Bus; a headless server usually has
/// none, and every call then fails with [`KeystoreErrorKind::Unavailable`].
#[derive(Default)]
pub struct OsKeychain {
    /// The kind of item this handle reads and writes; the platform store
    /// itself is process-wide.
    kind: ItemKind,
}

impl OsKeychain {
    /// A handle to this machine's credential store for master-key items.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle to this machine's credential store for items of `kind`.
    ///
    /// # Arguments
    ///
    /// * `kind` - decides the keychain service items are stored under.
    #[must_use]
    pub fn for_kind(kind: ItemKind) -> Self {
        Self { kind }
    }
}

impl Vault for OsKeychain {
    fn get(&self, item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>> {
        platform::get(self.kind, item)
    }

    fn set(&self, item: KeychainItemId, bytes: &[u8]) -> Result<()> {
        check_len(bytes)?;
        platform::set(self.kind, item, bytes)
    }

    fn delete(&self, item: KeychainItemId) -> Result<()> {
        platform::delete(self.kind, item)
    }
}

/// The Secret Service backend (Linux and other Unix desktops).
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
use secret_service as platform;

/// The native backends of macOS and Windows, through `keyring-core`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform {
    use std::sync::{Arc, Mutex, OnceLock};

    use zeroize::Zeroizing;

    use crate::{ItemKind, KeychainItemId, KeystoreError, KeystoreErrorKind, Result};

    /// The opened platform store, once a call has needed it.
    static STORE: OnceLock<Mutex<Option<Arc<keyring_core::CredentialStore>>>> = OnceLock::new();

    /// Opens (or returns the already opened) platform store.
    fn store() -> Result<Arc<keyring_core::CredentialStore>> {
        let mut guard = STORE.get_or_init(|| Mutex::new(None)).lock().map_err(|_| {
            KeystoreError::new(KeystoreErrorKind::Unavailable, "store lock poisoned")
        })?;
        if let Some(store) = guard.as_ref() {
            return Ok(Arc::clone(store));
        }
        let store = open()?;
        *guard = Some(Arc::clone(&store));
        Ok(store)
    }

    /// Opens this platform's native credential store.
    #[cfg(target_os = "macos")]
    fn open() -> Result<Arc<keyring_core::CredentialStore>> {
        let store = apple_native_keyring_store::keychain::Store::new().map_err(map_error)?;
        Ok(store)
    }

    /// Opens this platform's native credential store.
    #[cfg(target_os = "windows")]
    fn open() -> Result<Arc<keyring_core::CredentialStore>> {
        let store = windows_native_keyring_store::Store::new().map_err(map_error)?;
        Ok(store)
    }

    /// The entry for `item` under `kind`'s service.
    fn entry(kind: ItemKind, item: KeychainItemId) -> Result<keyring_core::Entry> {
        store()?
            .build(kind.service(), &item.to_string(), None)
            .map_err(map_error)
    }

    /// Reads an item's bytes.
    pub(crate) fn get(kind: ItemKind, item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>> {
        entry(kind, item)?
            .get_secret()
            .map(Zeroizing::new)
            .map_err(map_error)
    }

    /// Creates or replaces an item.
    pub(crate) fn set(kind: ItemKind, item: KeychainItemId, bytes: &[u8]) -> Result<()> {
        entry(kind, item)?.set_secret(bytes).map_err(map_error)
    }

    /// Deletes an item; a missing item is not an error.
    pub(crate) fn delete(kind: ItemKind, item: KeychainItemId) -> Result<()> {
        match entry(kind, item)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(err) => Err(map_error(err)),
        }
    }

    /// Maps a keyring error to a [`KeystoreError`] without copying any bytes
    /// it carries (`BadEncoding` and `BadDataFormat` hold the item's contents).
    fn map_error(err: keyring_core::Error) -> KeystoreError {
        use keyring_core::Error as E;
        match err {
            E::NoEntry => KeystoreError::new(KeystoreErrorKind::NotFound, "no such item"),
            E::NoStorageAccess(inner) => {
                KeystoreError::new(KeystoreErrorKind::AccessDenied, inner.to_string())
            }
            E::PlatformFailure(inner) => {
                KeystoreError::new(KeystoreErrorKind::Unavailable, inner.to_string())
            }
            E::NoDefaultStore => {
                KeystoreError::new(KeystoreErrorKind::Unavailable, "no credential store")
            }
            E::BadEncoding(_) | E::BadDataFormat(_, _) => KeystoreError::new(
                KeystoreErrorKind::Corrupt,
                "the item's contents are not in the expected format",
            ),
            E::BadStoreFormat(msg) => KeystoreError::new(KeystoreErrorKind::Corrupt, msg),
            E::TooLong(attr, limit) => KeystoreError::new(
                KeystoreErrorKind::Rejected,
                format!("{attr} is longer than {limit}"),
            ),
            E::Invalid(attr, reason) => {
                KeystoreError::new(KeystoreErrorKind::Rejected, format!("{attr}: {reason}"))
            }
            E::Ambiguous(found) => KeystoreError::new(
                KeystoreErrorKind::Rejected,
                format!("{} items match", found.len()),
            ),
            E::NotSupportedByStore(what) => KeystoreError::new(KeystoreErrorKind::Rejected, what),
            // `keyring_core::Error` is `#[non_exhaustive]`; a variant added
            // upstream is reported without its contents.
            _ => KeystoreError::new(
                KeystoreErrorKind::Unavailable,
                "unrecognized keychain error",
            ),
        }
    }
}

/// No native credential store is supported on this platform.
#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    )
)))]
mod platform {
    use zeroize::Zeroizing;

    use crate::{ItemKind, KeychainItemId, KeystoreError, KeystoreErrorKind, Result};

    /// The error every call returns here.
    fn unsupported() -> KeystoreError {
        KeystoreError::new(
            KeystoreErrorKind::Unavailable,
            "this platform has no supported credential store",
        )
    }

    /// Always fails: no store.
    pub(crate) fn get(_kind: ItemKind, _item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>> {
        Err(unsupported())
    }

    /// Always fails: no store.
    pub(crate) fn set(_kind: ItemKind, _item: KeychainItemId, _bytes: &[u8]) -> Result<()> {
        Err(unsupported())
    }

    /// Always fails: no store.
    pub(crate) fn delete(_kind: ItemKind, _item: KeychainItemId) -> Result<()> {
        Err(unsupported())
    }
}

/// Items kept in process memory: for tests, and for hosts that must not touch
/// the OS keychain.
#[derive(Default)]
pub struct MemoryVault {
    /// Item bytes by id.
    items: Mutex<HashMap<KeychainItemId, Zeroizing<Vec<u8>>>>,
    /// When set, every call fails with this kind (simulates a locked or
    /// missing keychain).
    failure: Mutex<Option<KeystoreErrorKind>>,
}

impl MemoryVault {
    /// An empty vault.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every later call fail with `kind` (`None` restores normal use).
    ///
    /// # Arguments
    ///
    /// * `kind` - the failure to report, or `None`.
    pub fn fail_with(&self, kind: Option<KeystoreErrorKind>) {
        if let Ok(mut guard) = self.failure.lock() {
            *guard = kind;
        }
    }

    /// How many items the vault holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.lock().map(|items| items.len()).unwrap_or(0)
    }

    /// True when the vault holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fails with the configured kind, if any.
    fn check(&self) -> Result<()> {
        match self.failure.lock().ok().and_then(|g| g.clone()) {
            Some(kind) => Err(KeystoreError::new(kind, "simulated keychain failure")),
            None => Ok(()),
        }
    }

    /// The item map, or an error when its lock is poisoned.
    fn items(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<KeychainItemId, Zeroizing<Vec<u8>>>>> {
        self.items
            .lock()
            .map_err(|_| KeystoreError::new(KeystoreErrorKind::Unavailable, "vault lock poisoned"))
    }
}

impl Vault for MemoryVault {
    fn get(&self, item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>> {
        self.check()?;
        self.items()?
            .get(&item)
            .cloned()
            .ok_or_else(|| KeystoreError::new(KeystoreErrorKind::NotFound, "no such item"))
    }

    fn set(&self, item: KeychainItemId, bytes: &[u8]) -> Result<()> {
        self.check()?;
        check_len(bytes)?;
        self.items()?.insert(item, Zeroizing::new(bytes.to_vec()));
        Ok(())
    }

    fn delete(&self, item: KeychainItemId) -> Result<()> {
        self.check()?;
        self.items()?.remove(&item);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_round_trips() {
        for (store, magic) in [
            (PointerStore::Keychain, b"BCK3"),
            (PointerStore::SystemdCreds, b"BCK4"),
        ] {
            let pointer = KeychainPointer {
                store,
                item: KeychainItemId::random(),
                fingerprint: [7u8; FINGERPRINT_LEN],
            };
            let encoded = pointer.encode();
            assert_eq!(encoded.len(), POINTER_LEN);
            assert_eq!(&encoded[..4], magic);
            assert_eq!(KeychainPointer::parse(&encoded), Ok(pointer));
            assert_eq!(PointerStore::for_header(&encoded), Some(store));
        }
        assert_eq!(PointerStore::for_header(b"BCK"), None);
        assert_eq!(PointerStore::for_header(b"BCK2...."), None);
    }

    #[test]
    fn other_headers_are_not_pointers() {
        assert_eq!(
            KeychainPointer::parse(b"BCK1aaaa"),
            Err(PointerError::NotAPointer)
        );
        assert_eq!(KeychainPointer::parse(b""), Err(PointerError::NotAPointer));
        assert_eq!(
            KeychainPointer::parse(b"BC"),
            Err(PointerError::NotAPointer)
        );
    }

    #[test]
    fn a_pointer_of_the_wrong_length_is_refused() {
        let encoded = KeychainPointer {
            store: PointerStore::Keychain,
            item: KeychainItemId::random(),
            fingerprint: [1u8; FINGERPRINT_LEN],
        }
        .encode();
        for len in [4, 19, 20, POINTER_LEN - 1] {
            assert_eq!(
                KeychainPointer::parse(&encoded[..len]),
                Err(PointerError::BadLength(len))
            );
        }
        let mut long = encoded.to_vec();
        long.push(0);
        assert_eq!(
            KeychainPointer::parse(&long),
            Err(PointerError::BadLength(POINTER_LEN + 1))
        );
    }

    #[test]
    fn the_memory_vault_stores_replaces_and_deletes() {
        let vault = MemoryVault::new();
        let item = KeychainItemId::random();
        assert_eq!(
            vault.get(item).map(|_| ()).map_err(|e| e.kind),
            Err(KeystoreErrorKind::NotFound)
        );
        vault.set(item, b"one").unwrap();
        vault.set(item, b"two").unwrap();
        assert_eq!(vault.get(item).unwrap().as_slice(), b"two");
        assert_eq!(vault.len(), 1);
        vault.delete(item).unwrap();
        vault.delete(item).unwrap();
        assert!(vault.is_empty());
    }

    #[test]
    fn the_memory_vault_can_simulate_a_locked_keychain() {
        let vault = MemoryVault::new();
        vault.fail_with(Some(KeystoreErrorKind::AccessDenied));
        let err = vault.set(KeychainItemId::random(), b"x").unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::AccessDenied);
        vault.fail_with(None);
        vault.set(KeychainItemId::random(), b"x").unwrap();
    }

    #[test]
    fn oversized_items_are_refused() {
        let vault = MemoryVault::new();
        let err = vault
            .set(KeychainItemId::random(), &[0u8; MAX_ITEM_LEN + 1])
            .unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::Rejected);
    }

    #[test]
    fn error_messages_name_the_kind() {
        let err = KeystoreError::new(KeystoreErrorKind::Unavailable, "no session bus");
        assert_eq!(err.to_string(), "OS keychain unavailable: no session bus");
    }

    /// Writes, reads and deletes a real item in this machine's keychain.
    /// Ignored by default: it needs an unlocked keychain (a desktop session,
    /// or `gnome-keyring` on a session bus).
    #[test]
    #[ignore = "needs an unlocked OS keychain"]
    fn the_os_keychain_round_trips_an_item() {
        let vault = OsKeychain::new();
        let item = KeychainItemId::random();
        vault.set(item, b"bookclerk keystore test item").unwrap();
        assert_eq!(
            vault.get(item).unwrap().as_slice(),
            b"bookclerk keystore test item"
        );
        vault.delete(item).unwrap();
        assert_eq!(
            vault.get(item).map(|_| ()).map_err(|e| e.kind),
            Err(KeystoreErrorKind::NotFound)
        );
    }
}
