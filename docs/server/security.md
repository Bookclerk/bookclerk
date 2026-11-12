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

### Store connections a branch receives

A person who joins a branch may bring their store connections. Only stores
that declare their sign-in shareable travel, and the user must tick the
box for it on the library. The library seals the accounts' secrets to the
joining branch's own box key, so the code, the browser and any proxy on
the way never see them. The branch re-seals each secret with its own
`master.key` before it writes it. Neither side logs the values, and the
library audits what it sent, without them. Each operator can stop a
member from connecting store accounts on their server. Details:
[Store connections that follow the person](cluster-hosts.md#store-connections-that-follow-the-person).

### Leave the web app's policy in place

The web app is served with a Content-Security-Policy. Pages run only
Bookclerk's own scripts, load images only from Bookclerk, talk only to
Bookclerk, and can't be framed by another site. Covers, store icons and
profile pictures are downloaded by Bookclerk and served from its own
address ([How covers are served](integrations.md#how-covers-are-served)).
If your proxy adds its own `Content-Security-Policy` header, it must still
allow Bookclerk's own scripts, images and connections. A stricter header,
such as `script-src 'none'`, stops the app from loading.

### Other sites can't use Bookclerk's responses

Every response from Bookclerk, the web app, the API, covers and error pages
alike, tells browsers that it belongs to Bookclerk alone:

- Other sites can't show Bookclerk's covers, scripts or files in their pages
  (`Cross-Origin-Resource-Policy: same-origin`).
- Scripts on other sites can't read Bookclerk's answers: no response carries
  an `Access-Control-Allow-Origin` header, so browsers refuse the read.
- The app runs in its own browser process, apart from any page that opens it
  or that it opens (`Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`).
- No site can frame it (`X-Frame-Options: DENY`), and browsers keep to the
  declared content type (`X-Content-Type-Options: nosniff`).

These headers bind browsers only. The CLI, other hosts of your library and
integrations that call the API from their own servers are unaffected.

> [!NOTE]
> A proxy that adds `Access-Control-Allow-Origin`, or loosens
> `Cross-Origin-Resource-Policy`, opens your library to other sites. Leave
> these headers as Bookclerk sends them.

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

Plugin requests identify as `bookclerk/<version> (+https://bookclerk.app)`.
A plugin that follows a store's app API can ask to send its own
`User-Agent` to that store's hosts; the consent form lists those hosts and
the plugin's reason, and you can refuse each one. See
[User-Agent](../plugins/capabilities.md#user-agent).

Install only the plugins you use, and undeploy the rest.

## Backups

Keep recovery points, and a copy of `master.key`, somewhere other than the
server. See [Backups, updates, import and export](backups.md).

## Next steps

- [Running Bookclerk](operations.md): run the server as a service.
- [Sign-in](sign-in.md): passkeys, single sign-on and second factors.
- [Backups, updates, import and export](backups.md): recovery points.
