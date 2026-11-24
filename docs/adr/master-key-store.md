# ADR: The library key can live in the OS keychain

- **Status:** Accepted (decisions made with the maintainer on 2026-11-20;
  decision 6 on 2026-11-21; decision 7 on 2026-11-23). All seven
  decisions are built
- **Date:** 2026-11-20
- **Related:** [Control plane](control-plane.md),
  [Library catalog network](library-catalog-network.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md)

## Context

One random 32-byte data-encryption key (DEK) seals every row of
`encrypted_secrets`: store sign-ins, the operator token, plugin secrets,
library links. It lives in `$BOOKCLERK_FILES_DIR/master.key`, either raw
(`BCK1`) or wrapped with the auth password (`BCK2`). The database stores the
key's SHA-256 fingerprint in `library_identity`, and a branch whose key
doesn't match refuses to start. Shared branches hold the same key.

Most installs run `BCK1`, because nothing asks for a password on a desktop
or a home server. A copy of the files directory (a backup, a synced folder,
a stolen disk) then carries the key next to `library.db`, which is every
store sign-in.

Every desktop operating system has a credential store tied to the user's
login: Keychain Services on macOS, Credential Manager on Windows, and the
Secret Service (GNOME Keyring, KWallet) on Linux desktops. A headless Linux
server usually has none: there is no session D-Bus, and nothing unlocks a
collection at boot.

The keychain protects the key against copies of the files directory. It
does not protect it against code running as the same OS user, which can ask
the keychain for the item as Bookclerk does.

## Decision

### 1. Two key stores, chosen per install

The key is kept in one of two stores:

| Store | Where the key is | `master.key` holds |
| --- | --- | --- |
| `file` | `master.key` | The envelope: `BCK1`, or `BCK2` under the auth password |
| `keychain` | One OS keychain item | A `BCK3` pointer to the item |

The keychain item holds the same `BCK1` or `BCK2` envelope the file would,
so a password still wraps the key when one is set, and no new key format
exists. Items live under the service `app.bookclerk.master-key`, with a
random per-install UUID as the account name, so two installs for one OS user
keep separate items.

`[auth].key_store = "file" | "keychain"` in `config.toml` chooses the store
for new keys. Unset, the desktop window uses the keychain and `bookclerkd`
and the CLI use the file. The desktop default falls back to the file, with a
warning, when the keychain can't take the key; a configured store is
required.

The `bookclerk-keystore` crate reads, writes and deletes items: through the
`keyring-core` native stores on macOS and Windows, and on Linux by speaking
the Secret Service API over `zbus` itself. It opens a `plain` session there,
because the encrypted session needs AES-CBC and the store-free host scan
forbids block ciphers in the hosts. The session bus is the user's own local
socket, the same boundary the keychain protects. An existing item is
updated where it is, a missing default collection is created, and a
keychain prompt that gets no answer in five minutes fails the call. Its
errors never carry item bytes.

### 2. `master.key` always says where the key is

On a keychain install `master.key` is a 52-byte `BCK3` record: the header,
the item's UUID and the key's SHA-256 fingerprint. It holds no key
material. Loading follows whatever `master.key` says, whatever the setting:

- `BCK1` and `BCK2` load from the file, as before.
- `BCK3` reads the item and checks that the key inside matches the
  fingerprint.

A missing item, a locked or unreachable keychain, or a fingerprint mismatch
is an error that names the cause. Bookclerk never mints a key while
`master.key` exists, so a pointer can't silently become a new, empty
library.

Putting the location in the key's own record rather than only in the
setting means a lost `config.toml` or a changed setting can't send a branch
looking in the wrong place.

### 3. Existing keys move on purpose

`bookclerk config master-key move keychain|file` moves the key and records
the choice as `[auth].key_store`. The key's bytes don't change, so sealed
secrets stay readable. Moving to the keychain writes the item, reads it
back, then replaces `master.key` with the pointer atomically. Moving to the
file writes the envelope over the pointer, reloads it, then deletes the
item.

The desktop window moves a key still in `master.key` into the keychain when
it starts, unless `[auth].key_store` is set. `bookclerkd` never moves a key
by itself: a server operator chooses that moment.

A shared branch joining a library writes the library's key into its own
store, so shared branches of one library can keep the key in different
stores. The fingerprint check is the same.

### 4. Recovery copies

A key in the keychain is no longer in a copy of the files directory.
`bookclerk config master-key export --out <file>` writes a `BCK2` recovery
copy under a password read from stdin, and `bookclerk config master-key
import --from <file>` restores it (or a copied `master.key`) on an install
that has no key, into that install's store.

The desktop window offers to save a recovery copy when a library is
started in it. Skipping it shows a warning, and Settings keeps a reminder
until one is saved. The answer is recorded per key, beside `master.key`,
so a restored or replaced key asks again.

### 5. Other per-install secrets stay sealed in the database

The desktop window's patron session and other per-install rows stay sealed
in `encrypted_secrets`. With the key in the keychain they are protected by
it already, and one item per install means one macOS access prompt.

### 6. A `systemd-creds` store for headless Linux

A third store, `key_store = "systemd-creds"`, for servers with no Secret
Service. Packaging can't seal the key: it doesn't exist when the package
installs, and a shared-branch join replaces it at runtime. So:

- **Bookclerk seals it itself.** On start, `bookclerkd` moves a key still
  in `master.key` into a credential sealed with `systemd-creds --user`
  (systemd 256 or later; TPM2 when present, the host key otherwise),
  written beside `master.key` as `master-key-<id>.cred` and read back
  before `master.key` becomes a `BCK4` pointer. The tool runs with a
  cleared environment and the envelope goes over stdin, never argv.
- **Administrators can seal it themselves.** `bookclerk config master-key
  credential` writes the envelope, unchanged, to a pipe for
  `systemd-creds encrypt --name=bookclerk-master-key`. The unit loads it
  with `LoadCredentialEncrypted=`, and a move into `systemd-creds` adopts
  that credential (same bytes) instead of sealing its own copy. This
  works on older systemd and with the system credential store.
- **Sealing always falls back.** If sealing fails for any reason, the key
  stays in `master.key` (password-wrapped when a password is set) with a
  warning, even when `key_store` is set. An explicit `config master-key
  move systemd-creds` still fails instead, so the operator sees why.
- **Unsealing fails closed.** A sealed key that no longer opens is an
  error, as for a missing keychain item; the fix is the recovery copy.

### 7. A hosted database's token follows the key

A hosted database (Cloudflare D1) needs its API token before the library
opens, so it can't be sealed in `encrypted_secrets`. The token that
`bookclerk config database setup d1` mints is sealed with the DEK and kept
in the same store as the key, even though it is already sealed:

- On a file install `database.credential` holds the sealed token (`BCD1`).
- On a keychain or `systemd-creds` install the sealed token goes in an
  item of its own, and `database.credential` holds only a pointer (`BCD3`
  or `BCD4`) with the DEK's fingerprint.
- The items use their own names: keychain service
  `app.bookclerk.database-credential`, `systemd-creds` credential
  `bookclerk-database-credential` and files
  `database-credential-<id>.cred`, so a unit-provided master key is never
  read as the token, nor the other way round.
- `config master-key move` takes the credential along. Reading fails
  closed, like the key: a missing item or a token sealed under another key
  is an error. `BOOKCLERK_D1_API_TOKEN` still overrides it.
- A shared branch receives the library's token in its sealed bootstrap,
  beside the key, and stores it the same way in its own store. A later
  setup keeps that token unless told to replace it, so shared branches
  aren't cut off.

## Consequences

- A copied files directory no longer exposes sealed secrets on keychain
  installs.
- Backups of a keychain install need a recovery copy (or the keychain's own
  backup). The backup docs say so.
- Unsigned development builds on macOS see an "allow access" prompt after
  each rebuild, because Keychain Services ties access to the code signature.
  `cargo dev` runs `bookclerkd`, whose default is the file.
- No new untrusted-input boundary: `master.key`, `database.credential`
  and their store items are written by the branch itself, and `import --from` reads a file the
  operator chose, with the same trust as `master.key` (whoever can hand the
  CLI that file can already replace `master.key`). The pointer and envelope
  parsers still have strict header and length tests.
- `bookclerk-keystore` joins the security surface.
- Headless Linux servers can seal the key with `systemd-creds` (decision
  6), or keep the password-wrapped file. A sealed key doesn't survive a
  new TPM or a reinstall, so the recovery copy matters as much there as on
  a desktop.
- The keychain does not help against code running as the same OS user, and
  removing a shared branch still doesn't take the key back; re-keying the
  library is a separate decision.
