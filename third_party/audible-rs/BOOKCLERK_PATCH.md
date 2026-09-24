# Bookclerk patch of audible-rs

Upstream: https://github.com/mkb79/audible-rs @ `5a28f507072022ae7fd7f95a62e3bdc5e515d678`

## Why this tree exists

Bookclerk's library DB uses SeaORM 2 (`sqlx-sqlite`), which depends on
`libsqlite3-sys` `<0.38`. Upstream `audible-rs` pins `rusqlite` 0.40
(`libsqlite3-sys` `^0.38`). Cargo cannot link two `links = "sqlite3"`
crates, even when SeaORM's sqlx driver is unused.

This vendored tree is identical to the pinned upstream revision except:

1. `rusqlite` is lowered to `0.37` so the workspace shares one
   `libsqlite3-sys` (`0.35`) **if** the optional `cli` feature is enabled.
   Bookclerk's Audible plugin does not enable `cli`, so audible-rs does
   not link SQLite in the plugin binary.
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
4. Library-only default features: the `audible` CLI, local library DB,
   config.toml, plugin broker, terminal progress, and unused `totp-rs`
   (Amazon OTP is completed in the browser; this crate never generated
   codes) compile only with `--features cli`. Bookclerk depends with
   `default-features = false`.
5. `auth/authfile.rs` unit test `wrong_password_fails` derives its mismatch
   from the literal fixture password at runtime (analyzer hygiene for a
   second hard-coded secret). This is not a security improvement.
6. RSA private-key blinding (RUSTSEC-2023-0071 / `rsa` 0.9.10). Defense in
   depth only — not a substitute for a patched `rsa` release:
   - Widevine `Cdm::parse_license` unwraps `SignedMessage.session_key` with
     `RsaPrivateKey::decrypt_blinded` (same RSA-OAEP/SHA-1 as `decrypt`).
   - Audible request signing uses `pkcs1v15::SigningKey::sign_with_rng`.
     PKCS#1 v1.5 stays deterministic; the RNG is blinding only.
   - Widevine challenge signing uses `pss::BlindedSigningKey` (SHA-1, salt
     length = digest length). `pss::SigningKey::sign_with_rng` leaves the
     private operation unblinded.
7. `downloader` lib tests import `url::Url` (used by the annotation-base
   lockstep test; the type was previously unresolved under `--lib` tests).

Re-vendor when bumping the `audible-rs` git rev in the workspace
`Cargo.toml`, then re-apply these patches.
