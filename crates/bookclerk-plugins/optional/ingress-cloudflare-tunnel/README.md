# Cloudflare Tunnel

An `ingress` plugin (id `cloudflare_tunnel`, `[ingress] kind = "relay"`)
that serves the library on a hostname routed to a Cloudflare Tunnel in the
operator's Cloudflare account. Nothing at home is opened to the internet,
and no `cloudflared` program runs: the plugin is a tunnel connector itself
and dials out to Cloudflare's edge over one TLS connection the host holds.

> [!NOTE]
> This plugin is experimental. It speaks the protocol `cloudflared` uses,
> which Cloudflare does not document, and it has not been run against the
> live edge from Bookclerk's CI.

## How it works

- `start` opens a host-held `tls://` stream to
  `region1.v2.argotunnel.com:7844` (then `region2`), asking for the
  edge's name `h2.cftunnel.com` (the stream's `serverName`). It trusts
  Cloudflare's Origin CA roots first, as `cloudflared` does (`src/ca.ts`),
  then the public roots.
- On that stream the plugin serves HTTP/2 (`src/h2.ts`, `src/hpack.ts`):
  the edge is the client. The edge first opens a control stream, on which
  the plugin registers the connection with the tunnel token over Cap'n
  Proto RPC (`src/capnp.ts`, `src/rpc.ts`). `start` answers
  `https://<hostname>` once the edge confirms the registration.
- Each public request arrives as an HTTP/2 stream, with TLS already ended
  at Cloudflare's edge and the client's address in `cf-connecting-ip`. The
  plugin hands it to the library through `env.ORIGIN` and writes the
  answer back, its fields serialized the way `cloudflared` sends them
  (`src/agent.ts`, `src/serve.ts`).
- A WebSocket request (a branch's socket, a clerk's live link) opens a
  socket to the library through `env.ORIGIN.connect`. The stream then
  carries the client's frames to that socket and the library's messages
  back (`src/ws.ts`). Either side's close reaches the other.
- `health` sends an HTTP/2 ping and reports the tunnel serving once the
  edge answers it. When the connection ended, `health` connects again.
- `stop` closes the connection, and the edge takes this connection of the
  tunnel offline.

The session (its streams, windows, header table and half-read requests)
lives in the plugin's memory. When the plugin starts again while the host
still holds the connection, the session is gone, so the plugin closes that
connection and opens a new one, which registers again and replaces the old
registration.

## Set it up

1. Install and approve the plugin.
2. Run the guided setup (`src/provision.ts`): create an API token with
   **Account: Cloudflare Tunnel: Edit**, **Zone: Zone: Read** and **Zone:
   DNS: Edit**, then in **Settings** open the plugin's instance, set
   `hostname` (such as `books.example.com`), paste the token and select
   **Run guided setup**. The setup creates a remotely managed tunnel named
   `bookclerk-<hostname>` (or finds the one the stored token runs, or one
   of that name), routes the hostname to it, points the hostname at
   `<tunnel id>.cfargotunnel.com` with a proxied `CNAME` carrying the
   comment `Kept by Bookclerk (cloudflare_tunnel)`, and keeps only the
   tunnel's token. It refuses a hostname that holds other records or a
   `CNAME` to something other than a tunnel.
3. Or set it up by hand: create a tunnel in the dashboard (**Networks →
   Tunnels → Create a tunnel**, type Cloudflared), add the hostname to it
   with any service, such as `http://localhost:8787` (the plugin sends
   every request to the library and ignores the tunnel's routing rules),
   and set the settings:

   ```sh
   bookclerk plugins config set cloudflare_tunnel hostname books.example.com
   bookclerk plugins config set cloudflare_tunnel token --secret   # reads the token from stdin
   ```

4. Deploy and activate it:

   ```sh
   bookclerk plugins deploy cloudflare_tunnel
   bookclerk ingress activate cloudflare_tunnel
   ```

The plugin's network grant is TCP to the two edge regions on port 7844,
and `api.cloudflare.com` for the guided setup. `h2.cftunnel.com` is on
the grant too, because the host checks that a stream's server name is
granted.

Before the instance is deleted, **Remove what setup created** deletes the
`CNAME` and the tunnel. The tunnel's token can't call the API, so it needs
a pasted token like the setup's.

## Limits

- One connection to the edge, where `cloudflared` keeps four. When it
  drops, the tunnel is offline until the plugin reconnects.
- workerd hands the plugin one message at a time, so requests are served
  one after another, and a request body (up to 32 MiB) is read whole
  before it goes to the library.
- A response is read into memory up to 4 MiB. A larger one is sent in
  pieces: the plugin fetches each later megabyte from the library with a
  `Range` request when the edge has room for it. That works for a `GET`
  whose answer gives its length, allows byte ranges and is not
  compressed, as audio and downloads do. Any other response over 4 MiB is
  answered `502`.
- The session holds at most 64 MiB for all streams together; past that, a
  request is answered `503`. At most 256 streams are open at once.
- A WebSocket message may be up to 256 KiB either way. No extension (such
  as compression) is negotiated.
- Raw TCP streams (WARP, private networks) are answered `502`. Tunnels on
  a dedicated endpoint (such as FedRAMP) and the tunnel's own routing
  rules are not supported.
- The tunnel token is a secret: it goes to Cloudflare only inside the TLS
  stream, and never into logs.

## Code from cloudflared

`src/rpc.ts` follows the message layouts of `cloudflared`'s
`tunnelrpc/proto/tunnelrpc.capnp` and Cap'n Proto's `rpc.capnp`, the
stream handling in `src/agent.ts` follows `cloudflared`'s
`connection/http2.go` and `connection/header.go`, and `src/ca.ts` copies
the root certificates from `tlsconfig/cloudflare_ca.go`. `cloudflared` is
Apache-2.0, Copyright Cloudflare, Inc.; this plugin is AGPL-3.0-or-later,
which the Apache-2.0 license allows.

## Development

```sh
npm ci
npm run build       # src/*.ts -> modules/*.js (commit both)
npm run typecheck
npm test            # unit tests, Node's HTTP/2 client as the edge, and the fuzz corpora
```
