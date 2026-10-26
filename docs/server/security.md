# Security checklist

Found a vulnerability? See the [security policy](../../SECURITY.md) for how to
report it privately.

What to set up before a Bookclerk server is reachable from anywhere but the
computer it runs on, and where each piece is explained in detail.

## Keys and secrets

- **`master.key`** in the files directory seals every stored secret (store
  sign-ins, the operator token, plugin secrets) in the database. Set
  `BOOKCLERK_AUTH_PASSWORD` (or `[auth].password`) so the key is wrapped at
  rest; an existing key is wrapped with `bookclerk config master-key wrap`.
  Back the key up with the database: without it, sealed values cannot be
  read. Details: [Database](database.md) and
  [Configuration → Bootstrap files](configuration.md#bootstrap-files).
- **The operator token** is the key to the server itself. Show it with
  `bookclerk daemon token` and replace it with `bookclerk daemon token
  rotate`. Once an Owner exists, the web sign-in no longer accepts it; Owners
  use elevation instead. Details: [Operations](operations.md#daemon-bookclerkd).
- **Secrets never go on a command line.** Store passwords are entered in
  Accounts, plugin secrets with `bookclerk plugins config set --secret`
  (read from stdin). Logs and diagnostics reports redact every registered
  secret, and an upload is abandoned if one would remain.
  Details: [Diagnostics](diagnostics.md#redaction--privacy).

## Network

- The daemon listens on loopback only by default (`127.0.0.1:8787` and
  `[::1]:8787`). Before you change `daemon.listen`, put a TLS-terminating
  reverse proxy in front and set `integrations.public_origin` to the
  `https://` address, so session cookies are `Secure` and sign-in links use
  that origin. Details: [Operations → Reverse proxy](operations.md#reverse-proxy--tls).
- List your proxy in `daemon.trusted_proxies`; otherwise forwarded headers
  are ignored and sign-in throttling uses the proxy's address.
- Hosts of one library talk to each other over TLS except on loopback.
  Details: [Libraries and branches](cluster-hosts.md).

## Sign-in

- Ask for a second factor on password sign-ins (**Settings → Sign-in**, the
  `require_second_factor` policy), or use passkeys or single sign-on.
  Details: [Sign-in](sign-in.md).
- Give people the lowest role that works for them. Only Owners manage
  sign-in options and other Administrators. See
  [Sharing with your household](../guide/sharing.md).

## Plugins and media

- Keep `plugins.isolation` and `media.isolation` at `"required"` (the
  default). Every plugin then runs in its own jail, and untrusted audio is
  only decoded by the confined media worker; when either helper is missing
  the work is refused rather than run unconfined.
  Details: [Managing plugins → Isolation modes](plugins.md#isolation-modes)
  and [Media worker](media.md).
- Read what a plugin asks for before approving it. A plugin reaches only the
  web addresses and network ranges its approved grant names, and a plugin
  that later asks for more stops running until it is approved again.
  Details: [Capabilities and consent](../plugins/capabilities.md).
- Install only the plugins you use, and undeploy the rest.

## Backups

Keep recovery points and a copy of `master.key` somewhere other than the
server. See [Backups, updates, import and export](backups.md).
