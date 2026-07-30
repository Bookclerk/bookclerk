# Bookclerk patch of audible-rs

Upstream: https://github.com/mkb79/audible-rs @ `5a28f507072022ae7fd7f95a62e3bdc5e515d678`

## Why this tree exists

Bookclerk's library DB uses SeaORM 2 (`sqlx-sqlite`), which depends on
`libsqlite3-sys` `<0.38`. Upstream `audible-rs` pins `rusqlite` 0.40
(`libsqlite3-sys` `^0.38`). Cargo cannot link two `links = "sqlite3"`
crates, even when SeaORM's sqlx driver is unused.

This vendored tree is identical to the pinned upstream revision except:

1. `rusqlite` is lowered to `0.37` so the workspace shares one
   `libsqlite3-sys` (`0.35`).
2. Auth-file salt/nonce generation uses `MaybeUninit` + `OsRng` instead of
   zero-initialized arrays (avoids CodeQL "hard-coded cryptographic value"
   false positives on the temporary `[0u8; N]` buffers).
3. `Authenticator` gains DB-backed write-back support (Bookclerk
   `encrypted_secrets` migration):
   - `WriteBackFn` async callback type alias
     (`Fn(Value, Option<MergeScope>) -> Pin<Box<dyn Future<…>>>`).
   - `WriteBack` is now an enum (`File { path, protection, password }` /
     `Callback(WriteBackFn)`) — `load_file` still sets the `File` variant.
   - `Authenticator::load_from_bytes` loads a new-format envelope from raw
     bytes without configuring any write-back.
   - `Authenticator::set_write_back_fn` registers an async callback awaited
     by `save` / `save_merged` (no nested `block_on`). `save_merged` passes
     `Some(MergeScope)` so DB callbacks can RMW-merge via `merge_auth_json`.

Re-vendor when bumping the `audible-rs` git rev in the workspace
`Cargo.toml`, then re-apply these patches.
