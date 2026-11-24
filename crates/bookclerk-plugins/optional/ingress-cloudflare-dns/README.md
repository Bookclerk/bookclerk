# Cloudflare DNS

An `ingress` plugin (id `cloudflare_dns`, `[ingress] kind = "direct"`)
that keeps a host name on Cloudflare DNS pointed at this computer: dynamic
DNS. Traffic comes straight to the daemon's own TLS listener with its ACME
certificate, so TLS ends at home and no provider can read it.

## How it works

- `start` and `health` ask the address echo for this computer's public
  addresses: `https://ipv4.icanhazip.com/` answers over IPv4 only and
  `https://ipv6.icanhazip.com/` over IPv6 only, each with the address it
  saw. The plugin keeps only public addresses (no private, shared CGNAT,
  loopback, link-local, documentation or other special-purpose ranges;
  `src/address.ts`).
- The plugin finds the host name's zone through the Cloudflare API, trying
  the name and then each parent, and points the name's `A` record at the
  IPv4 address and its `AAAA` record at the IPv6 address (`src/dns.ts`).
  The records are unproxied, with a 60-second TTL and the comment
  `Kept by Bookclerk (cloudflare_dns)`. An existing record of the same type
  is taken over and corrected.
- `start` answers `https://<hostname>` once the records point here.
- The host calls `health` every minute. Each call asks the echo again (two
  small requests) and follows a change at once. While the addresses stay
  the same, the records are checked against Cloudflare only every 30
  minutes, so a record edited by hand is put back within half an hour.
- `stop` leaves the records in place, so starting again serves at once.

When a family is turned off in the settings, the plugin deletes that
family's record, but only one that carries its comment. When an enabled
family has no address for a while (an IPv6 outage, say), its record is
left as it is and `health` says why it is not published.

## Set it up

1. Have the domain on Cloudflare (its nameservers point there).
2. Make the daemon serve its own TLS: in `config.toml`, set
   `[daemon.node_api] mode = "acme"`, `acme_contact`, `tls_listen`, and
   `integrations.public_origin = "https://books.example.com"` (the same
   name). See
   [Node API transport](../../../../docs/server/configuration.md#node-api-transport).
3. Let the internet reach `tls_listen` on port 443 of the name. For IPv4,
   forward TCP port 443 on the router to `tls_listen` (such as
   `0.0.0.0:8443`). IPv6 has no port forwarding, so to turn `ipv6` on the
   daemon itself must listen on port 443 (`tls_listen = "[::]:443"`, which
   needs permission to bind a low port), and the router's firewall must
   let inbound TCP 443 reach this computer. The certificate is issued over
   TLS-ALPN-01, which needs port 443 on every published address.
4. Install and approve the plugin, then run the guided setup
   (`src/provision.ts`): create an API token with **Zone: Zone: Read** and
   **Zone: DNS: Edit** on the zone and **Account: Account API Tokens:
   Edit**, then in **Settings** open the plugin's instance, set
   `hostname`, paste the token and select **Run guided setup**. The setup
   finds the zone and mints a token named
   `Bookclerk cloudflare_dns <hostname>` that can read that zone and edit
   its DNS records, and nothing else. The plugin keeps that token and its
   id (`api_token_id`); the pasted one is not stored. A second run mints a
   new token and deletes the old one.

   To set it up by hand, create a token with **Zone: Zone: Read** and
   **Zone: DNS: Edit** on the zone and set the settings:

   ```sh
   bookclerk plugins config set cloudflare_dns hostname books.example.com
   bookclerk plugins config set cloudflare_dns api_token --secret   # reads the token from stdin
   ```

   `ipv4` is on and `ipv6` off by default. Turn IPv6 on, once the
   daemon listens on port 443 (step 3), with
   `bookclerk plugins config set cloudflare_dns ipv6 true`.

5. Deploy and activate it:

   ```sh
   bookclerk plugins deploy cloudflare_dns
   bookclerk ingress activate cloudflare_dns
   ```

The plugin's network grant is `api.cloudflare.com` and the two echo names.

Before the instance is deleted, **Remove what setup created** deletes the
records carrying the plugin's comment (with the stored token) and the
minted token (which needs a pasted token with **Account API Tokens:
Edit**).

## Limits

Tested against Cloudflare's Free plan, 2026-11.

- Behind carrier-grade NAT (many mobile and some fibre providers) the
  IPv4 address the echo sees belongs to the provider, and port forwarding
  can't reach this computer. Use IPv6, or a relay ingress instead.
- The name must not be a `CNAME`, and must hold at most one `A` and one
  `AAAA` record; the plugin says so rather than guess which to keep.
- Records stay unproxied: proxying would end TLS at Cloudflare and break
  the daemon's own certificate.
- The plugin does not answer ACME DNS-01 challenges; the daemon gets its
  certificate over TLS-ALPN-01 on port 443.
- The API token is a secret: it goes only to `api.cloudflare.com`, and
  never into logs or errors.

## Development

```sh
npm ci
npm run build       # src/*.ts -> modules/*.js (commit both)
npm run typecheck
npm test            # unit tests with a fake Cloudflare API, and the fuzz corpora
```
