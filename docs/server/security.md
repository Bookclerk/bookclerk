# Security checklist

Work through this list before a Bookclerk server can be reached from any
computer but its own. Each item links to the page that explains it.

> [!IMPORTANT]
> Found a vulnerability? Report it privately, as the
> [security policy](../../SECURITY.md) describes.

## Keys and secrets

### Wrap `master.key` with a password

`master.key` in the files directory seals every secret Bookclerk stores:
store sign-ins, the operator token and plugin secrets.

- Set `BOOKCLERK_AUTH_PASSWORD` (or `[auth].password`) so the key is
  wrapped at rest.
- To wrap a key that already exists, run
  `bookclerk config master-key wrap`.

> [!WARNING]
> Back up `master.key` together with the database. Without the key,
> sealed values cannot be read.

Details: [Database](database.md) and
[Configuration → Bootstrap files](configuration.md#bootstrap-files).

### Guard the operator token

The operator token is the key to the server itself.

- Show it with `bookclerk daemon token`.
- Replace it with `bookclerk daemon token rotate`.

Once an Owner exists, the web sign-in no longer accepts the token. Owners
use elevation instead. Details:
[Operations](operations.md#daemon-bookclerkd).

### Keep secrets off the command line

- Enter store passwords in **Accounts**.
- Set plugin secrets with `bookclerk plugins config set --secret`, which
  reads the value from stdin.

Logs and diagnostics reports redact every registered secret. An upload is
abandoned if a secret would remain in it. Details:
[Diagnostics](diagnostics.md#redaction--privacy).

## Network

### Put a TLS proxy in front first

By default the daemon listens on loopback only (`127.0.0.1:8787` and
`[::1]:8787`). Before you change `daemon.listen`:

1. Put a reverse proxy that terminates TLS in front of Bookclerk.
2. Set `integrations.public_origin` to the proxy's `https://` address.
   Session cookies are then `Secure`, and sign-in links use that address.
3. List the proxy in `daemon.trusted_proxies`. Otherwise Bookclerk ignores
   forwarded headers, and sign-in throttling sees only the proxy's
   address.

Details: [Operations → Reverse proxy](operations.md#reverse-proxy--tls).

### Branches talk over TLS

Hosts of one library talk to each other over TLS, except on loopback.
Details: [Libraries and branches](cluster-hosts.md).

## Sign-in

- **Ask for a second factor** on password sign-ins: **Settings → Sign-in**,
  the `require_second_factor` policy. Passkeys and single sign-on are
  other strong options. Details: [Sign-in](sign-in.md).
- **Give people the lowest role that works.** Only Owners and the operator
  manage sign-in options and other Administrators. See
  [Sharing with your household](../guide/sharing.md).

## Plugins and media

### Keep isolation required

Leave `plugins.isolation` and `media.isolation` at `"required"`, the
default:

- Every plugin runs in its own jail.
- Untrusted audio is decoded only by the confined media worker.
- When either helper is missing, Bookclerk refuses the work instead of
  running it unconfined.

Details: [Managing plugins → Isolation modes](plugins.md#isolation-modes)
and [Media worker](media.md).

### Read what a plugin asks for

A plugin reaches only the web addresses and network ranges its approved
grant names. If it later asks for more (a new version, or a changed
manifest), it stops running until you approve it again. Details:
[Capabilities and consent](../plugins/capabilities.md).

Install only the plugins you use, and undeploy the rest.

## Backups

Keep recovery points, and a copy of `master.key`, somewhere other than the
server. See [Backups, updates, import and export](backups.md).

## Next steps

- [Running Bookclerk](operations.md): run the server as a service.
- [Sign-in](sign-in.md): passkeys, single sign-on and second factors.
- [Backups, updates, import and export](backups.md): recovery points.
