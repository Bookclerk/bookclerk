# One TLS backend

Every TLS connection Bookclerk makes or accepts uses
[rustls](https://github.com/rustls/rustls) with the **aws-lc-rs** crypto
provider. No first-party code uses OpenSSL, `native-tls` or rustls's `ring`
provider.

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
> The embeddings stack (`fastembed` and `hf-hub` in `bookclerk-discover`)
> still brings `native-tls` and `ring` until it leaves the default host
> build.

## Checking the tree

These commands list who brings a second crypto library into the build. Each
should print nothing but the embeddings stack named above:

```sh
cargo tree --workspace -e normal,build -i ring
cargo tree --workspace -e normal,build -i openssl-sys
```

When a new dependency brings either one, turn on its aws-lc-rs or
"no provider" feature instead, and hand it `bookclerk_tls::crypto_provider()`
if it needs one at run time.

## Next steps

- [Node API and the live socket](node-api.md): the TLS the hosts of one
  library use to talk.
- [Database](../server/database.md): Postgres connection settings.
