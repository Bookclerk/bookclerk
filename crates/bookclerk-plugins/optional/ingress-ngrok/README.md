# ngrok

An `ingress` plugin (id `ngrok`, `[ingress] kind = "relay"`) that gives the
library a public `https://` address on a domain in the operator's ngrok
account. Nothing at home is opened to the internet, and no ngrok program
runs: the plugin is an ngrok agent itself and dials out to ngrok over one
TLS connection the host holds.

## How it works

- `start` opens a host-held `tls://` stream to
  `connect.ngrok-agent.com:443`. That endpoint is signed by ngrok's own
  root certificate, not a public one, so the stream trusts only that
  certificate (`src/ca.ts`, passed as the stream's `trustAnchors`).
- Inside the stream the plugin speaks muxado, ngrok's stream multiplexer
  (`src/mux.ts`). It authenticates with the authtoken on stream 3 and binds
  an HTTPS endpoint on the configured domain on stream 5
  (`src/messages.ts`). `start` answers `https://<domain>` once ngrok
  confirms the bind.
- For each public request, ngrok opens a stream that carries a short
  header (the client's address) and the client's HTTP/1.1 bytes, with TLS
  already ended at ngrok's edge. The plugin reads one request
  (`src/http1.ts`), hands it to the library through `env.ORIGIN` with the
  client's address and writes the answer back with `Connection: close`
  (`src/agent.ts`, `src/serve.ts`).
- A WebSocket upgrade (a branch's socket, a clerk's live link) opens a
  socket to the library through `env.ORIGIN.connect`. The plugin answers
  the client's handshake with `101 Switching Protocols`, then reads the
  client's frames from the stream and passes each message to that socket,
  and writes the library's messages back as frames (`src/ws.ts`). Either
  side's close reaches the other, and the stream ends with it.
- ngrok heartbeats the connection about every 10 seconds; the plugin
  echoes each beat. `health` sends a beat of its own and reports the
  endpoint serving once ngrok echoes it. When the session ended, `health`
  connects again.
- `stop` closes the connection, and ngrok takes the endpoint offline.

The session (its streams, windows and half-read requests) lives in the
plugin's memory. When the plugin starts again while the host still holds
the connection, the session is gone, so the plugin closes that connection
and opens a new one, which authenticates and binds again. ngrok's
heartbeats keep the plugin awake while it is connected.

## Set it up

1. In the ngrok dashboard, copy your authtoken and note your domain: the
   free static domain under **Domains** (such as `example.ngrok-free.app`)
   or one you reserved.
2. Install and approve the plugin, then set its settings:

   ```sh
   bookclerk plugins config set ngrok domain example.ngrok-free.app
   bookclerk plugins config set ngrok authtoken --secret   # reads the authtoken from stdin
   ```

3. Deploy and activate it:

   ```sh
   bookclerk plugins deploy ngrok
   bookclerk ingress activate ngrok
   ```

The plugin's only network grant is TCP to `connect.ngrok-agent.com:443`.

## Limits

- workerd hands the plugin one message at a time, and a request's streams
  end with the call that made them. So requests are served one after
  another, and a request body (up to 32 MiB) is read whole before it goes
  to the library.
- A response is read into memory up to 4 MiB. A larger one is sent in
  pieces: the plugin fetches each later megabyte from the library with a
  `Range` request when ngrok has room for it. That works for a `GET` whose
  answer gives its length, allows byte ranges and is not compressed, as
  audio and downloads do. Any other response over 4 MiB is answered `502`.
- The session holds at most 64 MiB for all streams together (bodies being
  read and answers waiting for ngrok); past that, a request is answered
  `503`.
- A WebSocket message may be up to 256 KiB either way, and up to 256
  WebSockets are open at once. No extension (such as compression) is
  negotiated. A client that leaves more than 4 MiB of the library's
  messages unread is closed with `1008`.
- Upgrades other than WebSocket are answered `501`.
- TLS passthrough endpoints, TCP endpoints and ngrok's traffic policies are
  not supported. ngrok's restart, stop and update requests are declined;
  stopping the endpoint from the dashboard takes it offline until the next
  `health` check reconnects.

## Code from ngrok

`src/mux.ts` ports muxado's framing from `golang.ngrok.com/muxado/v2`
v2.0.2, and `src/messages.ts` and `src/ca.ts` port message shapes and the
root certificate from `golang.ngrok.com/ngrok` v1.12.1. Both are MIT,
Copyright 2022 ngrok, Inc.; this plugin is AGPL-3.0-or-later, which the
MIT license allows.

## Development

```sh
npm ci
npm run build       # src/*.ts -> modules/*.js (commit both)
npm run typecheck
npm test            # unit tests, the fake ngrok session and the fuzz corpora
```
