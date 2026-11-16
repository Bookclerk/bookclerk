# One TLS backend

Every TLS connection Bookclerk makes or accepts uses
[rustls](https://github.com/rustls/rustls) with the **aws-lc-rs** crypto
provider. No first-party TLS connection uses OpenSSL, `native-tls` or
rustls's `ring` provider.

aws-lc-rs is the provider rustls enables by default, and the only one that
offers the hybrid post-quantum key exchange (`X25519MLKEM768`), which rustls
prefers when the other side supports it. Building one crypto library also
keeps cold builds shorter: each extra provider is another C build.

## Where the provider is set

The hosts name the provider in one place, the `bookclerk-tls` crate:

| Need | Use |
| --- | --- |
| A rustls `ClientConfig` or `ServerConfig` (the node API listener and client) | `bookclerk_tls::crypto_provider()` |
| A blocking HTTP request (diagnostics upload, plugin catalog, workerd download, CLI to daemon) | `use bookclerk_tls::ureq;`, then `ureq::get(url)` as usual |
| A custom `ureq` agent | `.tls_config(bookclerk_tls::ureq_tls_config())` on its config builder |

`bookclerk_tls::ureq` is the `ureq` crate with one change: its request
functions (`get`, `post` and the rest) send through a shared agent that
carries the provider. The workspace builds `ureq` with `rustls-no-provider`,
so `ureq`'s own default agent has no provider and would panic on its first
HTTPS request. Host crates therefore depend on `bookclerk-tls`, not on
`ureq` directly.

Other clients pick aws-lc-rs through their features:

- `reqwest` 0.13 (storefront guests, plugin SDK) uses its default
  `rustls` feature, which is aws-lc-rs. The plugin SDK also installs
  aws-lc-rs as the process default in guests.
- The AWS SDK (S3 destination) uses `default-https-client`, which is rustls
  on aws-lc-rs.
- Postgres connections (sqlx, under SeaORM) use sqlx's
  `tls-rustls-aws-lc-rs` feature. `bookclerk-db-guest` enables it, so every
  database guest has it. SeaORM's own `runtime-tokio-rustls` feature is left
  off because it selects sqlx's `ring` provider.

> [!NOTE]
> The ONNX embeddings stack (`fastembed` and `hf-hub` in
> `bookclerk-discover`) still brings `native-tls`, OpenSSL and `ring`. It is
> built only with the opt-in `onnx-embeddings` feature of `bookclerkd` and
> `bookclerk-cli`, never in a default host build.

## Reusing connections

A TLS handshake costs far more CPU than the request it opens: about
2.4 ms per request on a client built for the call, against about 0.2 ms
on a client whose connection is kept alive (measured through an HTTPS
proxy on one core; a small VPS is several times slower). So HTTP clients
are built once and reused:

- In a plugin guest, `bookclerk_plugin_sdk::http::Client::new()` hands out
  clones of one client per guest process, so every call shares one
  connection pool. That client has no cookie jar and no default headers,
  and it never leaves the guest's own process, so nothing carries between
  plugins, accounts or requests. A client with cookies, its own headers
  or its own redirect rules is built with `Client::builder()` and keeps
  its own pool; build it once per signed-in account, not per call.
- In the host, the public Audible catalog client
  (`bookclerk_audible_catalog::public_http_client`), the companion-file
  download client and the OIDC client (no redirects) are each built once
  per process.

## Checking the tree

These commands list who brings a second crypto library into the build:

```sh
cargo tree --workspace -e normal,build -i ring
cargo tree --workspace -e normal,build -i openssl-sys
```

Both should print nothing for a default build. With `--features
onnx-embeddings` they list only the embeddings stack named above. Passkeys
(`webauthn-rs` 0.6 in `bookclerkd`) verify signatures and attestation
certificates with pure-Rust RustCrypto crates, not OpenSSL.

When a new dependency brings either one, turn on its aws-lc-rs or
"no provider" feature instead, and hand it `bookclerk_tls::crypto_provider()`
if it needs one at run time.

## Next steps

- [Node API and the live socket](node-api.md): the TLS the hosts of one
  library use to talk.
- [Database](../server/database.md): Postgres connection settings.
