# Cloudflare relay

An `ingress` plugin (id `cloudflare_relay`, `[ingress] kind = "relay"`)
that gives the library a public `https://` address through a small relay
Worker in the operator's own Cloudflare account. Nothing at home is opened
to the internet: the plugin dials out to the relay, and the relay passes
each public request back over that connection.

## How it works

- `start` deploys the relay Worker with one Durable Object (`Relay`)
  through the Cloudflare API, turns on its `workers.dev` route and opens
  six host-held WebSockets to it. It answers with
  `https://<worker_name>.<account subdomain>.workers.dev` once the relay
  holds them.
- For each public request, the relay sends a short notice on an idle
  socket. The plugin pulls the request from the relay, hands it to the
  library through `env.ORIGIN` with the client address Cloudflare reported,
  and pushes the answer back. Bodies stream both ways.
- A WebSocket upgrade takes the same path. The plugin opens a socket to
  the library with `env.ORIGIN.connect` and, when the library accepts,
  pushes a `101` naming that socket; the library's refusal goes back as
  an ordinary answer. The connection's messages then travel as binary
  live frames on the relay socket that served the upgrade, so live
  connections use none of the plugin's outbound sockets. The relay keeps
  each client's socket under the hibernation API, so it can sleep while
  connections are idle. When a relay socket closes, the connections on it
  close too and the client reconnects.
- `health` asks the relay how many sockets it holds and reopens any that
  closed. When every socket has closed, it looks the relay's address up
  through the Cloudflare API and reconnects. `stop` closes the sockets and leaves the Worker deployed, so
  starting again keeps the same name.

The relay and the plugin share `src/protocol.ts`. Every relay path needs a
bearer token derived from the API token (an HMAC), deployed with the
Worker as a secret binding. A new API token gives a new relay token on the
next start.

## Set it up

1. In the Cloudflare dashboard, open **Workers & Pages** once, so the
   account has a `workers.dev` subdomain.
2. Install and approve the plugin.
3. Run the guided setup (`src/provision.ts`): create an API token with
   **Account: Workers Scripts: Edit** and **Account: Account API Tokens:
   Edit**, then in **Settings** open the plugin's instance, paste it and
   select **Run guided setup**. The setup finds the account (set
   `account_id` when the token reaches more than one), checks the Worker
   name, and mints a token named `Bookclerk cloudflare_relay <worker>`
   that can deploy Workers in that account and nothing else. The plugin
   keeps that token and its id (`api_token_id`); the pasted one is not
   stored. A second run mints a new token and deletes the old one.

   To set it up by hand, create a token with **Workers Scripts: Edit** and
   set `account_id` and `api_token`:

   ```sh
   bookclerk plugins config set cloudflare_relay account_id <32-hex account id>
   bookclerk plugins config set cloudflare_relay api_token --secret   # reads the token from stdin
   ```

   `worker_name` (default `bookclerk-relay`) is the first label of the
   address. A Worker of that name that Bookclerk did not create is left
   alone: the setup refuses it, and `start` fails with a message saying
   so.
4. Deploy it on this computer: `bookclerk plugins deploy cloudflare_relay`.
5. Make it the library's ingress: `bookclerk ingress activate
   cloudflare_relay`. See [Public address](../../../../docs/server/ingress.md).

Before the instance is deleted, **Remove what setup created** deletes the
relay Worker (with the stored token) and the minted token (which needs a
pasted token with **Account API Tokens: Edit**).

> [!WARNING]
> Cloudflare ends TLS at its edge, so it can read the traffic it carries:
> sign-ins, cookies and audio. Use dynamic DNS instead when TLS must end at
> home.

## Limits

These are the relay's own limits and those of Cloudflare's Workers Free
plan as Cloudflare documents them. The relay's tests run it under Node with
fake sockets; it has not yet been run against a live Cloudflare account.

- The plugin serves six requests at once, one per socket; more wait at the
  relay for up to 30 seconds, then get `503`. A request the library does
  not start answering within 100 seconds gets `504`.
- A socket stays busy until the answer's body has reached the client, so a
  long download holds one of the six.
- Request bodies are capped at 100 MB by Cloudflare's Free plan.
- A live connection's messages are at most 256 KiB less a few bytes of
  framing (`MAX_LIVE_MESSAGE_BYTES`); a larger one closes the connection
  with `1009`. The host holds at most 256 live connections per instance.
- The relay drops `Content-Length` from answers and streams them, so a
  client sees no total size.
- A custom domain is not set up by the plugin yet; the address is always
  on `workers.dev`.

## Develop

`src/` is the source and `modules/` is what ships. `npm run build` compiles
`src/`, writes `src/relay-source.ts` (the relay Worker's emitted modules as
text, for the upload) and compiles again; CI fails when the committed
`modules/` differs. `relay-worker.js` is uploaded to Cloudflare and never
loaded by the plugin.

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../crates/bookclerk-plugins/optional/ingress-cloudflare
npm ci
npm run typecheck
npm run build
npm test
```

`test/relay.test.ts` runs the relay hub and the plugin's request handling
together under Node, with fake sockets and a fake library;
`test/live.test.ts` does the same for upgrades and live frames, and
`test/worker.test.ts` drives the relay Worker's Durable Object with a fake
state.
`test/fuzz.test.ts` fuzzes the relay protocol (see
[Fuzzing](../../../../docs/contributing/fuzzing.md#cloudflare-relay-guest)).
