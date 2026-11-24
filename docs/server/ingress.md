# Public address

An ingress plugin gives the library an address on the internet without
port forwarding, a reverse proxy or a certificate of your own. Branches
and clerks away from home reach the library there.

There are two kinds of ingress plugin:

- A **relay** keeps an outbound connection to a provider (such as a
  relay Worker in your own Cloudflare account). Requests arrive over that
  connection and the plugin hands them to the library. TLS ends at the
  provider's edge.
- A **direct** plugin keeps a DNS name pointing at this computer.
  Traffic comes to the built-in TLS listener (`acme` mode), so TLS ends
  at home.

The design is in the
[ingress plugins ADR](../adr/ingress-plugins.md). Plugin authors start
at [Public addresses](../plugins/abi.md#public-addresses-ingress).

## Before you start

- **Claim the library first.** Ingress stays off until an Owner account
  exists, so first-run setup is never public.
- **Keep operator auth on.** Ingress stays off while
  `daemon.auth.enabled = false`, because public traffic would reach the
  library without signing in.
- **Run the daemon as a server.** Only `bookclerkd` serves a public
  address; the desktop window does not.

## Turn ingress on

1. Install and approve the ingress plugin, then deploy it on the
   computer that runs the library (see [Managing plugins](plugins.md)).
2. Make it the library's active ingress:

   ```sh
   bookclerk ingress activate <plugin>
   ```

   `<plugin>` is a plugin key (its default instance) or a plugin instance
   UUID. A library has at most one active ingress.
3. Check that it is serving:

   ```sh
   bookclerk ingress status
   ```

   `serving as` shows the public origin once the plugin reports it.

The plugin starts within a few seconds. The first origin it reports is
adopted as the library's public origin and stored in the library
database, so it survives restarts and needs no `config.toml` change.

Turn it off with `bookclerk ingress deactivate`. The adopted origin is
kept, so turning the same provider back on serves under the same name.

The same settings are on the web API as `GET` and `POST
/api/config/ingress` (the operator, or an elevated Owner).

## What the public address serves

Choose what public traffic may reach:

```sh
bookclerk ingress exposure branches   # the default
bookclerk ingress exposure library
```

| Exposure | Serves |
| --- | --- |
| `branches` (default) | The node API, the clerk and copy routes, sign-in (password, passkey, authenticator app and OIDC), the branch and clerk consent screens, and the web app's pages. Nothing else under `/api/`, and none of the legacy control routes (`/status`, `/scan`, `/acquire`, `/jobs`, `/integrations`). |
| `library` | The whole library, web app included. |

Some things never pass, whatever the exposure:

- the operator token, as a bearer token or through operator sign-in
  (`/api/auth/login`), and the operator session cookie;
- the tray handoff, operator elevation and first-Owner setup.

A refused request gets `403` with `"error": "operator_only"` or
`"error": "not_exposed"`.

> [!NOTE]
> Every request through a relay is treated as public `https` traffic,
> never as loopback. Its client address is the one the plugin reports
> from the provider's own connection metadata; forwarding headers in the
> request are dropped.

## Cloudflare relay

The first-party `cloudflare_relay` plugin deploys a small relay Worker into
your own Cloudflare account and serves the library at
`https://<worker_name>.<account subdomain>.workers.dev`. It needs a
Cloudflare account with a `workers.dev` subdomain (open **Workers & Pages**
in the dashboard once) and an API token with the **Workers Scripts: Edit**
permission.

1. Install and approve `cloudflare_relay`, then set its account and token:

   ```sh
   bookclerk plugins config set cloudflare_relay account_id <account id>
   bookclerk plugins config set cloudflare_relay api_token --secret
   ```

   `--secret` reads the token from standard input and seals it.
2. Deploy it on this computer and activate it:

   ```sh
   bookclerk plugins deploy cloudflare_relay
   bookclerk ingress activate cloudflare_relay
   ```

The relay serves six requests at once; more wait up to 30 seconds. Live
connections (a branch's socket, a clerk's live link) pass through it too,
riding on the same six connections, so they don't count against the
six. Stopping
the ingress leaves the Worker in your account, so turning it back on keeps
the same name. The plugin's
[README](../../crates/bookclerk-plugins/optional/ingress-cloudflare/README.md)
lists its limits.

> [!WARNING]
> Cloudflare ends TLS at its edge and can read the traffic it carries:
> sign-ins, cookies and audio.

## ngrok

The first-party `ngrok` plugin connects to ngrok as an agent and serves
the library on a domain in your ngrok account: the free static domain
every account gets (such as `example.ngrok-free.app`) or a domain you
reserved. It needs the domain and your authtoken from the ngrok
dashboard. No ngrok program runs on your computer: the plugin speaks the
agent protocol itself over one TLS connection the host holds.

1. Install and approve `ngrok`, then set its domain and authtoken:

   ```sh
   bookclerk plugins config set ngrok domain example.ngrok-free.app
   bookclerk plugins config set ngrok authtoken --secret
   ```

2. Deploy it on this computer and activate it:

   ```sh
   bookclerk plugins deploy ngrok
   bookclerk ingress activate ngrok
   ```

The library answers at `https://<domain>`. Requests are served one at a
time. A request body may be up to 32 MiB. A response over 4 MiB is sent
in pieces, so it must allow byte ranges and give its length, as audio
and downloads do. Live connections (a branch's socket, a clerk's live
link) pass through too: each one holds a WebSocket to the library while
it lasts, up to 256 at once, and a message may be up to 256 KiB. The
plugin's
[README](../../crates/bookclerk-plugins/optional/ingress-ngrok/README.md)
lists its limits.

> [!WARNING]
> ngrok ends TLS at its edge and can read the traffic it carries:
> sign-ins, cookies and audio.

## Cloudflare Tunnel

The first-party `cloudflare_tunnel` plugin serves the library on a
hostname routed to a Cloudflare Tunnel in your Cloudflare account. It
needs the hostname and the tunnel's token from the Cloudflare dashboard.
No `cloudflared` program runs on your computer: the plugin connects to
Cloudflare's edge as a tunnel connector itself, over one TLS connection
the host holds. It is experimental.

1. In the Cloudflare dashboard, create a tunnel and add a public hostname
   to it (such as `books.example.com`). Copy the tunnel's token, the long
   value after `cloudflared service install`.
2. Install and approve `cloudflare_tunnel`, then set its hostname and
   token:

   ```sh
   bookclerk plugins config set cloudflare_tunnel hostname books.example.com
   bookclerk plugins config set cloudflare_tunnel token --secret
   ```

3. Deploy it on this computer and activate it:

   ```sh
   bookclerk plugins deploy cloudflare_tunnel
   bookclerk ingress activate cloudflare_tunnel
   ```

The library answers at `https://<hostname>`. Every request on the
hostname goes to the library; the tunnel's own routing rules are ignored.
The limits are those of the ngrok plugin: one request at a time, bodies
up to 32 MiB, responses over 4 MiB in byte ranges, and WebSocket messages
up to 256 KiB. The plugin keeps one connection to the edge, where
`cloudflared` keeps four. Its
[README](../../crates/bookclerk-plugins/optional/ingress-cloudflare-tunnel/README.md)
lists its limits.

> [!WARNING]
> Cloudflare ends TLS at its edge and can read the traffic it carries:
> sign-ins, cookies and audio.

## Cloudflare DNS

The first-party `cloudflare_dns` plugin is the direct kind: dynamic DNS.
It keeps a host name on Cloudflare DNS pointed at this computer's public
address, and traffic comes straight to the daemon's own TLS listener, so
TLS ends at home. It needs a domain on Cloudflare, an API token, and a
port open at home.

1. Create a Cloudflare API token with the **Zone: Zone: Read** and
   **Zone: DNS: Edit** permissions on the domain's zone.
2. Set up the TLS listener in `config.toml`: `[daemon.node_api]
   mode = "acme"`, with `acme_contact` and `tls_listen`, and set
   `integrations.public_origin` to `https://` and the host name. See
   [Node API transport](configuration.md#node-api-transport).
3. Forward TCP port 443 on the router to `tls_listen`.
4. Install and approve `cloudflare_dns`, then set its host name and
   token:

   ```sh
   bookclerk plugins config set cloudflare_dns hostname books.example.com
   bookclerk plugins config set cloudflare_dns api_token --secret
   ```

5. Deploy it on this computer and activate it:

   ```sh
   bookclerk plugins deploy cloudflare_dns
   bookclerk ingress activate cloudflare_dns
   ```

The plugin keeps the name's `A` record current, checking this computer's
address every minute, and the daemon gets its certificate for the name
over ACME. IPv6 (an `AAAA` record) is off by default, because IPv6 has no
port forwarding: turn it on with `ipv6 true` once the daemon listens on
port 443 itself. Its
[README](../../crates/bookclerk-plugins/optional/ingress-cloudflare-dns/README.md)
covers carrier-grade NAT and its other limits.

> [!NOTE]
> Traffic on a direct address reaches the TLS listener, not a relay, so
> the exposure choice above does not apply to it: the TLS listener serves
> the whole library, as in `acme` mode without ingress.

## The library's name

`integrations.public_origin` in `config.toml` still wins when it is set.
Leave it unset to use a relay's origin. A direct plugin needs it set to
the same name, because the TLS listener takes its certificate name from
it when the daemon starts.

If the active plugin later reports a different name, the library keeps
serving under the old one and `bookclerk ingress status` shows the new
one as `reported origin`. Accept it with:

```sh
bookclerk ingress accept-origin
```

> [!WARNING]
> Renaming the library breaks passkeys registered under the old name and
> sign-in redirects that name it. Patrons sign in another way and add
> their passkeys again.

## Branches over the public address

Set `[daemon.node_api] mode = "tunnel"` so the node API accepts requests
the relay carries (see
[Configuration → Node API transport](configuration.md#node-api-transport)).
Branches verify the relay's edge certificate against the public roots,
as in `acme` and `proxy` mode; it can't be pinned.

Invite links name the public origin, from the daemon and from
`bookclerk nodes invite` alike.

## Not built yet

- The first-party ingress plugins are the Cloudflare relay, ngrok,
  Cloudflare Tunnel and Cloudflare DNS. Dynamic DNS with other providers
  comes later, and the relay can't use a custom domain yet.
- There is no web app screen yet; use the CLI or the web API.

## Next steps

- [Libraries and branches](branches.md): join a library and let branches
  in.
- [Security checklist](security.md): before you expose a server.
