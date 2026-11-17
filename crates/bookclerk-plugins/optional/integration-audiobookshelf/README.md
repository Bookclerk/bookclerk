# Audiobookshelf integration

Outbound Audiobookshelf plugin (scan, listening sync, credential return-visit).
A TypeScript guest that runs in `workerd`; setup is in
[docs/server/integrations.md](../../../../docs/server/integrations.md#audiobookshelf).

## Layout

| Module | Does |
| --- | --- |
| `src/index.ts` | The entrypoint classes (`remoteLibrary`, `oidc`, the default one with the `book_acquired` consumer, the socket handler and migrations) |
| `src/guest.ts` | What each entrypoint method does, with every binding passed in |
| `src/client.ts` | The Audiobookshelf REST client (no redirects, size and request caps) |
| `src/watch.ts` | Remembers which users were reported, in `LISTENING_STATE` |
| `src/listening.ts` | Remembers what listening progress was sent, in `LISTENING_STATE` |
| `src/keyed.ts` | Paged reads and chunked writes within the database's limits |
| `src/push.ts` | The Socket.io conversation, frame by frame |
| `src/hash.ts` | The hashed keys the database stores instead of user ids |
| `src/settings.ts` | Reads the instance settings and the API key |

`npm run build` compiles `src/` into the `modules/` that ships; both are
committed. `npm test` runs the unit tests. The database tests run under
every `bookclerk-db-testkit` profile, so set `BOOKCLERK_DB_TESTKIT` to the
built kit (`cargo build -p bookclerk-db-testkit`) or put it on `PATH`.

## Network

The manifest asks for any public website (`domains = ["*"]`), because the
server's address is a setting. The plugin only ever calls `base_url`. A
server on a private or loopback address needs the operator to add its host
and CIDR to the plugin's consent.

## New users

With `watch_users` on, the host polls the plugin for new Audiobookshelf
users and mints a claim ticket for each. The plugin also opens
Audiobookshelf's Socket.io channel through the host (`[sockets]`): the host
holds it while the plugin sleeps and answers its pings, and a `user_added`
message makes the plugin ask the host to poll at once. `src/push.ts` reads
the channel; `src/watch.ts` remembers which users were reported.

## OpenID Connect (Bookclerk as IdP)

Bookclerk hosts a core OIDC authorization server (`/.well-known/openid-configuration`,
`/oidc/*`) so ABS can use Bookclerk Users as the login source. Configure ABS
OpenID with:

- Issuer: Bookclerk `integrations.public_origin` (or the daemon base URL)
- Client id: `audiobookshelf` (plugin-provided public PKCE client; enable it
  under Settings → Sign-in). Redirect URIs are derived from this plugin’s
  `base_url` plus `/auth/openid/callback` (ABS’s own UI, default port 13378 —
  not Bookclerk’s 8787 listen port) and are not edited by hand.
- Auth method: none / PKCE S256

Consent and token minting require a **User** portal session. The shared operator
token cannot obtain user access tokens. Details: [docs/server/integrations.md](../../../../docs/server/integrations.md).
