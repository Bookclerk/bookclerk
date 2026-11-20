# ADR: Public addresses come from ingress plugins

- **Status:** Accepted (decisions made with the maintainer on 2026-11-20)
- **Date:** 2026-11-20
- **Related:** [Control plane](control-plane.md),
  [Library catalog network](library-catalog-network.md),
  [Storage destinations](storage-destinations.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md),
  [Plugin capabilities v3](plugin-capabilities-v3.md)

## Context

A core branch is an install that its own branches, and outside services,
can reach. Today an operator makes one by hand: a reverse proxy or a
forwarded port, a DNS name, a certificate, and `integrations.public_origin`
in `config.toml`. Most people hosting at home stop at one of those steps,
often because their connection has no public address at all (CGNAT).

Operators want one action that gives an install a public `https://` name.
That name must stay the same across restarts, because passkeys, sign-in
redirects, branch links and plugin webhooks are all bound to the public
origin, and a changed name breaks every one of them.

Several services can carry traffic to a home server without an open port,
and dynamic DNS services can publish a home address when a port can be
opened. Each has its own account, credential and wire protocol, and the
list will change. Plugins already isolate this kind of difference: they
run jailed, declare their network reach, keep their settings in the
database and sleep when idle.

## Decision

### 1. An `ingress` plugin family

Public addresses come from plugins in a new `ingress` family. An ingress
plugin is `runtime = "workerd"` and written in TypeScript where the
provider's protocol allows it, because workerd plugins package and ship
the same way on every platform. A native guest is the fallback only when a
protocol can't be carried by workerd's `fetch`, WebSocket and TCP
`connect()`.

No ingress plugin runs a third-party executable. A provider's client
protocol is implemented in the plugin.

### 2. Two kinds of provider

| Kind | Who carries the traffic | Needs |
| --- | --- | --- |
| **Relay** | The provider, over a connection the plugin dials out | Nothing open at home |
| **Direct** | The home connection; the provider publishes the address (dynamic DNS) | A forwarded port or a reachable IPv6 address |

A plugin declares its kind in `plugin.toml`. A library runs at most one
active ingress instance at a time, so it has exactly one public origin.

### 3. Relay traffic enters through a host binding, never loopback

A relay plugin never connects to the daemon's TCP listener. The host gives
it an `ORIGIN` binding in Workers service-binding style: the plugin turns
each request it receives from the provider into a `Request`, calls
`env.ORIGIN.fetch(request)`, and streams the `Response` back.

Every request on that binding is classified as a new transport, `tunnel`:

- it is public and `https`, so cookies are `Secure`;
- it is never loopback, so it never reaches operator-only paths such as
  the tray handoff, the welcome screen or operator elevation;
- the client address is the one the plugin reports from the provider's
  own metadata, never a forwarded header in the request.

The node API accepts `tunnel` as a transport mode next to `acme`,
`proxy`, `self_signed` and `loopback`. Branches verify the library
against the webpki roots, as in `acme` and `proxy` mode; a relay's edge
certificate can't be pinned.

### 4. Direct traffic uses the existing TLS listener

A direct plugin only keeps a DNS record pointing at the install's public
address. Traffic reaches the built-in TLS listener, so the transport is
today's `acme` mode and TLS ends at home. A direct plugin whose provider
has a DNS API may also answer ACME DNS-01 challenges through a `dns01`
method, so a certificate can be issued before port 443 is open and
wildcard names work.

### 5. The public origin is held in the database while ingress is on

`integrations.public_origin` in `config.toml` stays the operator's
override. When it is unset and an ingress instance is active, the origin
the plugin reports becomes the library's public origin, stored in the
database. The host parses that report into a validated origin type.
Changing the name is an explicit operator action that warns about
passkeys and redirects.

The origin feeds what already keys on it: the heartbeat's public origin,
so the install shows as a core branch and its contributors as remote
branches; invite links; sign-in redirects; and the per-plugin webhook
reachability check in the storage destinations record.

### 6. Exposure is guarded

The host refuses to turn ingress on until an Owner exists, so first-run
claiming is never public. The operator token is refused on the `tunnel`
transport. The operator chooses what is exposed: the whole library, or
only what branches need (the node API, sign-in and clerk routes).

### 7. Setup goes through the provisioning entrypoint

Providers that need resources created (a relay Worker, a DNS record, a
reserved name) use the provisioning entrypoint from the storage
destinations record: the host collects a short-lived grant or a pasted API
token, the plugin creates what it needs and returns a narrow credential,
and the host seals it and drops the grant. Cloud providers share one
account-linking screen across storage, database and ingress plugins.

### 8. First providers

1. **Cloudflare relay.** The plugin deploys a Bookclerk relay Worker with
   one Durable Object into the person's own Cloudflare account and holds
   an outbound WebSocket to it. The name is
   `<worker>.<account>.workers.dev`, or a custom domain. The same relay
   runs on `workerd` anywhere, for a self-hosted relay.
2. **ngrok.** The agent protocol is ported to TypeScript from the
   open-source `ngrok-rust` SDK. A native guest linking that SDK is the
   accepted fallback.
3. **Cloudflare Tunnel (experimental).** cloudflared's HTTP/2 transport
   and registration are ported to TypeScript. The protocol is not
   documented by Cloudflare and may change, so this provider is marked
   experimental.
4. **Dynamic DNS.** Cloudflare DNS first, then other providers with an
   HTTP API.

UDP, which QUIC transports and UPnP port mapping would need, is not part
of this record; it is evaluated separately.

## Consequences

- `plugin.capnp` gains an `ingress` entrypoint (start, health, stop and,
  for direct providers, `dns01`) and the `ORIGIN` binding, appended with
  the TypeScript SDK.
- Each ported protocol and the relay protocol are untrusted-input
  boundaries on both ends. Each ships a sans-IO parser, a seed corpus, a
  replay test and fuzzing, and is listed in
  [Fuzzing](../contributing/fuzzing.md).
- Relays cap request bodies (100 MB on Cloudflare's Free plan), so branch
  uploads are sent in parts below that size.
- A relay provider can read the HTTP traffic it carries: sign-ins,
  cookies and audio. Secrets that branches receive stay sealed to their
  own keys. The setup screen says this, and points to dynamic DNS for
  TLS that ends at home.
- Providers' plans, limits and terms change, so each plugin's README
  records the limits it was tested against and the date.
