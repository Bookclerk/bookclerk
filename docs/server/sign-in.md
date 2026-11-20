# Web app and sign-in reference

This page is the reference for the web app's roles, sign-in methods,
routes and screens. For a plain-language tour, see
[The web app](../guide/web-app.md).

The web app is a React single-page app (SPA) that `bookclerkd` serves. It
covers Discover, Wishlist, the library and account linking. It talks only
to Bookclerk's Rust HTTP API; TypeScript never hosts the API.

## Roles

| Role | Signs in with | Can do |
| --- | --- | --- |
| **Operator** | The operator token, until an Owner exists; then a sign-in link or Owner elevation | Everything on the server, except connect stores |
| **Owner** | Invite link, password, passkey, SSO or integration login | Administrator powers, plus elevation to operator and **Settings → Nodes** for the nodes they own |
| **Administrator** | Invite link, password, passkey, SSO or integration login | Member powers, plus acquire and scan for every store account, jobs, adding users and **Settings → Nodes** for the nodes they own |
| **Host** | Invite link, password, passkey, SSO or integration return visit | Member powers, plus **Status** and **Settings → Nodes** for the nodes they own |
| **Member** | Invite link, password, passkey, SSO or integration return visit | Discover, Wishlist, browsing the shared library, **Accounts**, scan and acquire for the store accounts linked to them (through the API), and their own **Settings → Account** |

### The operator

The operator runs the server. They get the full library, scan, acquire,
jobs, Discover, Wishlist, **Settings** (daemon, plugins, confinement) and
impersonation. They **cannot** connect stores, because stores belong to
people.

How the operator signs in:

- **Before an Owner exists**, paste the operator token from
  `bookclerk daemon token`.
- **After that**, the web app refuses the pasted token
  (`POST /api/auth/login` returns 403). Use the tray's
  **Copy sign-in link**, run `bookclerk login`, or sign in as an Owner and
  elevate.

The Bearer API keeps accepting the token. The sign-in link is a loopback
`GET /api/auth/tray-handoff?code=` URL with a one-time code (180 seconds by
default), never the durable token.

### Owners and elevation

An Owner can **elevate** to operator, with an IdP step-up, a passkey or
their password. Elevated, they get **Server** and **Plugins** settings and
impersonation. Administrators can add users but cannot elevate.

Wrong elevation passwords count toward the same lockout as other sign-in
attempts, both for the client address and for the Owner. After too many
misses, `POST /api/auth/elevate` answers `429` with a `Retry-After` header,
even from another address, until the lockout ends. A passkey or IdP step-up
still works meanwhile.

The Owner's count is the account's password lockout, which every password
re-check by a signed-in user shares: elevation, changing your password, and
the `current_password` that sensitive changes ask for once a session is
more than fifteen minutes old (authenticator, passkey, sign-in provider and
OIDC client changes). Misses on any of them add up, and while the account
is locked each answers `429`. A correct password clears the count.

## Sign-in methods

### Passwords and second factors

**TOTP** (an authenticator app) is a second factor for password sign-in.

- When TOTP is on, a password sign-in returns
  `{ mfa: { method: "totp", challenge_id } }` instead of a session cookie.
- Passkey sign-in never asks for a TOTP code.
- Enrolling shows a QR code, a setup key to copy, and an `otpauth://`
  **Open in authenticator app** link.

**Requiring a second factor.** Set `require_second_factor` in
**Settings → Sign-in**, in `[daemon.auth]`, or with
`BOOKCLERK_DAEMON_AUTH_REQUIRE_SECOND_FACTOR`. Then:

- A password sign-in needs a TOTP code, or the user signs in with a
  passkey.
- A user with neither still gets a session, so they can enroll. The web
  app shows a blocking dialog: set up a passkey or authenticator, or log
  out and finish later.
- Logging out doesn't lock the account.

A failed password sign-in returns
`{ error: "invalid_credentials", message: "Invalid login or password." }`.

### Passkeys

- Passkeys can have a name of up to 80 characters. An empty name shows as
  `Passkey`.
- The relying party is `integrations.public_origin`. Without one, it is
  the loopback origin the request came in on, and otherwise
  `http://localhost:8787`.
- Loopback IP addresses are rewritten to `localhost`, because a WebAuthn
  relying party can't be `127.0.0.1`.
- The web app gives up on a passkey prompt after 45 seconds, and skips
  passkeys in browsers without `PublicKeyCredential`, so people can fall
  back to a password.

> [!IMPORTANT]
> Open the web app at `http://localhost:…` (the tray does), not at the
> raw IP address. On a production server, set
> `integrations.public_origin`.

### Single sign-on into Bookclerk

People can sign in with Google, GitHub, Apple, Discord or a custom OpenID
Connect provider (configured in `[auth.oidc]` or **Settings → Sign-in**).
Bookclerk is the client.

- The social presets ask only for a client ID and secret, and turn on
  **Link by verified email** by default. **Advanced** adds group mapping.
- A custom provider gets the full OpenID form, with linking by email off.

### Bookclerk as an identity provider

Bookclerk can also be the identity provider for other apps, such as
Audiobookshelf.

- Each client card has an on/off switch, redirect URIs, public (PKCE) or
  confidential (the secret is shown once), refresh tokens, and the
  `openid`, `profile` and `email` scopes.
- Client secrets are sealed in `encrypted_secrets` and never shown again.
- An installed player plugin, such as Audiobookshelf, adds a read-only
  client whose callbacks come from that plugin's server address. Custom
  clients stay fully editable.
- With an empty `public_origin`, Bookclerk uses the loopback origin
  (`localhost` under the tray or `cargo dev`). Saving an empty origin
  clears a pin, so loopback detection keeps working.

> [!NOTE]
> Port **13378** is Audiobookshelf's own web UI. Bookclerk listens on
> **8787** by default (`daemon.listen`).

### The sign-in screen

`GET /api/auth/signin` (public) returns
`{ operator_token, oidc, integrations }`. The screen shows username and
password first, then SSO and integration logins as buttons. The
operator-token box appears only while `operator_token` is true, that is,
while no Owner is active.

The SSO buttons use each brand's own mark, colors and wording
(**Continue with {Brand}**), and follow the app's resolved theme rather
than the OS hint alone:

| Brand | Light theme | Dark theme |
| --- | --- | --- |
| Google | 2025 gradient Super G on white `#FFFFFF`, `#747775` stroke, `#1F1F1F` Google Sans Medium | `#131314` fill, `#8E918F` stroke, `#E3E3E3` label; the G stays on a white tile |
| Apple | Black button | White button, black logo and title |
| GitHub | Invertocat on `#1F2328` | Black Invertocat on white |
| Discord | Clyde on Blurple `#5865F2` | White Clyde on black |

## Sessions and storage

| Item | Detail |
| --- | --- |
| Operator token | Sealed in `encrypted_secrets`, or set by `BOOKCLERK_OPERATOR_TOKEN`. `bookclerk daemon token` prints it |
| Sign-in link | Tray **Copy sign-in link** or `bookclerk login`: a loopback `?code=` URL, valid 180 seconds by default; the tray then removes that URL if it is still on the clipboard |
| Operator cookie | `bookclerk_operator_session` (`Path=/`), also used by elevated Owners |
| Portal cookie | `bookclerk_portal_session` (`Path=/`), the session of a first-party user |
| Library card | A clerk's session, sent as `Authorization: Bookclerk-Clerk <token>`, never as a cookie (see [Library cards](#library-cards)) |
| Config | `[daemon.auth]` |
| Preferences | Stored in the database (`user_preferences`), not `config.toml` |

### Library cards

A clerk can show a person's books from a library without joining it
as a branch. It signs in as the person and holds a **library card**: a
session token in the person's session list, not a node.

> [!NOTE]
> A clerk is another Bookclerk install that a person uses to reach a
> library. The API and the code use the same word: the built-in OAuth
> client `bookclerk-clerk`, the `Authorization: Bookclerk-Clerk` scheme,
> the `/api/clerk/` routes and the `clerk` session kind. Only OAuth's own
> parameter names, such as `client_id`, keep their standard spelling.

1. The clerk sends the browser to the library's `/oidc/authorize`
   with the built-in public client `bookclerk-clerk`, the scope
   `library:member`, an S256 code challenge, a `state`, a loopback or
   `https://` redirect URI, and a `clerk_name`.
2. The library sends the request to `/clerk/approve`, which signs the
   person in and asks "Give *name* a library card for *library*?", where *name* is the name the clerk gave itself.
   Approving needs an active user and the node API transport policy (TLS,
   except on loopback). It does not need "Let new branches join".
3. The library sends the browser back with a single-use code that lives
   60 seconds.
4. The clerk redeems the code with its verifier at
   `POST /api/clerk/v1/token` and gets
   `{ token, library_id, user, idle_secs }`. The token is shown once; the
   library keeps only its hash. Any failed redeem spends the code.

The clerk then sends `Authorization: Bookclerk-Clerk <token>`:

- It works only on the routes any library session may use: the library,
  Discover, the wishlist, the request queue, preferences, the person's
  profile and sessions, and sign-out. Every other route, administration
  included, sees no credential and refuses it, whatever the person's
  role.
- It needs TLS, except on loopback, like the node API. Otherwise the
  route answers `403 transport_insecure`, or `403
  clerk_sign_in_unavailable` where this daemon doesn't serve remote
  installs.
- It is never a cookie, and a browser's session token is never a library
  card.
- Each use keeps it signed in for another 90 days. Sign-out
  (`POST /api/auth/logout`) ends it, and so does **Revoke** in the
  person's sessions or disabling the user. When the library can't end the
  card's session, sign-out answers 503 `sign_out_failed` and the card stays
  signed in.
- It belongs to the sign-in that approved it, so it is listed and revoked
  beside that browser session, including a sign-in through an integration
  such as Audiobookshelf.

`bookclerk-clerk` codes are never redeemed at `/oidc/token`, and
`bookclerk-clerk` can't be registered as a client.

### The clerk socket

With its library card, a clerk keeps one WebSocket open to the library at
`GET /api/clerk/v1/connect`, so it hears about changes instead of polling.
It shows the library's books to the person, and is not a branch link: the
library never sends work on it.

1. The clerk connects with `Authorization: Bookclerk-Clerk <token>` and
   the subprotocol `bookclerk-clerk.v1`, over TLS except on loopback.
2. The library answers `hello` with the topics the card hears about
   (`library` and `discover`), then `changed` whenever one of them moves.
3. The clerk asks for the library's books a page at a time with
   `{"v":1,"type":"books","after":""}`. The library answers `books.page`
   with up to 100 works, each with its identifiers and the library's
   editions of it, and the cursor for the next page (none on the last
   page). The pages list every book on the library's store accounts, as
   its own Library page lists them for a member. Each edition is
   `{"edition": {...}, "yours": true}` when it is on a store account
   linked to the card's person; `yours` is left out otherwise.
   An edition whose audio the library already holds also carries `copy`:
   `content_type` (a media type such as `audio/mp4`), `bitrate_kbps` and
   `source_store` (the store the file came from) when the library has
   probed the file, `lossless` when true, and `version`, a short opaque
   string that changes when the library's file does. `copy` is left out
   for an edition the library has not downloaded.
   After those, the pages go on with the books only the library's
   branches hold: editions a branch it trusts listed in the last two days
   (the ones Discover and search show as available through a branch) that
   no store account of the library holds. Each such edition has
   `"availability": "reachable"`, no formats, no `copy` and never `yours`,
   and the page doesn't say which branch lists it. A branch reading the
   library's books never gets these.
   A cursor is an opaque string of up to 64 letters, digits and `-`. The
   clerk sends it back unchanged as the next `after`.
   A frame it cannot read gets `error` with code `invalid`.
4. The clerk asks for the library's copy of an edition with
   `{"v":1,"type":"copy.grant","store":"…","marketplace":"…","product_id":"…"}`,
   naming the edition by a store listing from a page (`marketplace` is
   left out for a store without marketplaces). The library answers
   `copy.granted` with the same listing, a `copy_id`, a `token`,
   `expires_in_secs` (one hour), `copy` (as on a page) and `files`: each
   file's `name`, `role` and `size`, the primary audio first. The clerk
   downloads file `n` of that list with
   `GET /api/copies/v1/{copy_id}/files/{n}` and
   `Authorization: Bookclerk-Copy <token>`; one `Range` is honoured, so a
   broken download resumes. An edition the person may not take, or one
   the library holds no copy of, gets `error` with code `not_found`; the
   two are not told apart. `unavailable` means try again later.

Answers come in the order the frames were asked.

A clerk asks for a copy when one of its own acquires wants that edition,
and takes it when the library downloaded it from the same store, or when
it is better than the clerk's own download from another store (see
[When a branch takes the copy](branches.md#when-a-branch-takes-the-copy),
which a clerk follows too). The clerk keeps the token only while the
acquire runs and keeps it out of its own logs.

The copy token is good only while the card is: the library checks on every
download that the card is still signed in and its user is not disabled,
that the person may still see the edition and that the files are the ones
it listed. Once the card is signed out, every new download request is
refused; a file already being sent is not cut off. See
[Taking a copy from the library](branches.md#taking-a-copy-from-the-library)
for the download route's answers.

> [!NOTE]
> The token is frame text on this socket, where a branch's is sealed to
> its key: a clerk has no key of its own, and the socket is the card's own
> connection, which is TLS except on loopback. Treat it like the card. The library stores only its hash
> and keeps it out of its logs.

The library checks the card again every 30 seconds, and before every page
and every copy it grants,
and closes the socket with code 4401 once the card is signed out, revoked,
expired, its user disabled, or its person must enroll a second factor (a
new socket then gets 403 `mfa_enrollment_required`). A socket the clerk
leaves silent for 60 seconds is closed, as is one that has not taken a
frame within 10 seconds, and a library serves at most 512 clerk sockets at
once (more get 503 `clerk_sockets_busy`).

### Signing in with a library card

On the clerk, the operator or an Owner signs this install in to a
library. The daemon, never the browser, talks to the library:

1. `POST /api/config/clerk/inspect` `{ address }` reads the library's
   `GET /api/node/v1/info` under the node API transport policy. A
   self-signed library's fingerprint comes back to confirm.
2. `POST /api/config/clerk/start` `{ address, fingerprint?, clerk_name? }`
   reads the library again, checks the confirmed fingerprint, and answers
   the authorize URL to open. The name defaults to this machine's name.
   The verifier and `state` stay in the daemon's memory for 10 minutes;
   a restart forgets them. The redirect comes back to this install's
   `/clerk/callback`.
3. `POST /api/config/clerk/callback` `{ code, state }` redeems the code
   with the verifier, checks the answer is for the library that was
   inspected, and seals the card into the database (`encrypted_secrets`,
   kind `clerk_card`). A sign-in is used once, whatever the outcome.

Then the daemon keeps the library's clerk socket open with the card:

- It reads every page of the library's books on each connection, whenever
  the library sends `changed` for `library`, and every 30 minutes. Once the
  last page is in, it drops the books the read no longer named. A read cut
  short drops nothing.
- The books show in **Books in other libraries**
  (`GET /api/config/join/window`, which lists the libraries in `cards`),
  merged with the books of any library this install is a branch of. Each
  edition there carries `yours`, and the person's own are marked **Yours**.
  An edition only the library's branches hold also carries
  `requestable_work`, the library's own work id.
- A lost connection is dialed again after 5 seconds, doubling up to 5
  minutes.
- When the library ends the card (close 4401, or 401 on connecting), the
  card and its books are dropped here, and the library stays listed as
  `signed_out` so the person can sign in again.

`POST /api/config/clerk/request` `{ library_id, work_id, node_id? }`
asks the library for a book only its branches hold, with `work_id` taken
from `requestable_work`. The daemon sends the library's own member route,
`POST /api/branch-requests`, with the card, as the person's browser
session there would, so the library decides: the person's member controls
there, whether a branch it trusts lists the work, and which branch it
asks. The daemon reads only the status of the library's answer:

| Answer | Means |
| --- | --- |
| 200 `{ library_id }` | The library took the request. It shows there with the person's other requests. |
| 400 `invalid_request` | The work or node id is malformed. |
| 403 `not_allowed` | The library's member controls turn requesting books off for the person. |
| 404 `not_found` | This install holds no active card for that library. |
| 404 `not_listed` | No branch of the library lists the work now. |
| 502 `card_refused` | The library no longer accepts the card; sign in again. |
| 502 `unreachable` or `library_error` | The library couldn't be reached in 15 seconds, or answered with another error. |
| 503 `library_busy` | The library's job queue is full. |

`GET /api/config/clerk` lists the cards. `POST /api/config/clerk/sign-out`
`{ library_id }` ends the card at the library (`POST /api/auth/logout`
with the card) when it can be reached, then drops the card, its books
and the library from the list. The answer's `library_signed_out` says
whether the library ended its session too; when it is `false`, the
person can revoke the session from their session list there.

An install that is already an active branch of a library reads
that library's books through its link, so a card for it is refused
(409 `already_joined`). These routes are for the operator or an Owner.

A card can replace a branch instead. `POST /api/config/clerk/start`
`{ from_branch, clerk_name? }` (a library id instead of `address` and
`fingerprint`) signs in to a library this install is an active branch of,
with the address and pinned certificate of the branch link. Only the
operator may, as only the operator leaves a library (403 otherwise), and
the library must still answer with the same library id (409
`origin_mismatch`) and certificate (409 `fingerprint_unconfirmed`). The
callback gets the card from the library first, then leaves the branch as
`POST /api/config/join/leave` does, then seals the card. Both run on their
own task, so a request cut short still finishes them. The answer's
`left_branch.owner_notified` says whether the library was told. If the
person doesn't approve the card, the branch stays. The sign-in remembers
which enrollment it replaces: if this install left and joined the library
again before the callback, the callback answers 409 `enrollment_changed`
and keeps the new branch.

The other way, a branch join of a library this install holds a card for
signs the card out (see
[How the sign-in works](branches.md#how-the-sign-in-works)).

In the web app this is Settings → Library → **Library cards**: **Sign in
with a library card** reads the library, asks for the fingerprint and the
name, and sends the browser to the library's sign-in page. The library
sends it back to `/clerk/callback`, which posts the `code` and `state` to
`POST /api/config/clerk/callback` and removes them from the address bar.
Only a request that got no answer offers **Try again**; any answer ends
the pending sign-in. In **Books in other libraries**, a book only a
library's branches hold says "On a branch of" that library and has a
**Request** button. Each card in the list has **Sign out** (**Remove** once
the library ended it), and for the operator **Make this server a branch**.
A branch in Settings → Library has **Use a library card instead**. See
[Sign in with a library card](../guide/branches.md#sign-in-with-a-library-card).

## API endpoints

| Area | Endpoints |
| --- | --- |
| Who am I | `GET /api/auth/me` |
| First Owner | `POST /api/auth/bootstrap` (operator, once, while no Owner exists) |
| Users | `GET`/`POST /api/users`, `PATCH /api/users/{id}`, `POST /api/users/{id}/claim-ticket` (operator, Owner or Administrator) |
| Profile | `PATCH /api/auth/profile` `{ display_name?, email?, avatar_source? }`; `PUT`/`DELETE /api/auth/profile/avatar`; `GET /api/users/{id}/avatar` (any signed-in user) |
| Elevation | `POST /api/auth/elevate` `{ password }`, `DELETE /api/auth/elevate`, `GET /api/auth/oidc/elevate`, `POST /api/auth/passkeys/elevate/*` (Owner only) |
| SSO | `GET /api/auth/oidc/providers`, `/login`, `GET`/`POST /callback` |
| SSO settings | `GET`/`PUT /api/auth/oidc/config` (providers, `public_origin`, `detected_origin`, issuer URLs; Owner or operator) |
| Bookclerk as IdP | `GET`/`POST`/`PUT`/`DELETE /api/auth/oidc/clients` |
| Passkeys | `GET`/`POST`/`DELETE /api/auth/passkeys…` (sign-in, register, elevate) |
| TOTP | `POST /api/auth/totp/enroll/begin` and `/finish`, `GET`/`DELETE /api/auth/totp`, `POST /api/auth/totp/login` |
| Second-factor policy | `GET`/`PUT /api/auth/mfa-policy` `{ require_second_factor }` (Owner or operator) |
| Preferences | `GET`/`PATCH /api/preferences`: `default_view`, `disabled_shelves`, `theme` (subject `user:{id}`, `portal:{identity_id}` or `operator`) |
| Portal | `/api/portal/*` (Accounts, invite claims) |
| Library cards | `GET`/`POST /api/auth/clerk-consent` (the consent page, any signed-in user), `POST /api/clerk/v1/token` (the clerk redeems its code), `GET /api/clerk/v1/connect` (the clerk socket) |
| Library cards (clerk side) | `GET /api/config/clerk`, `POST /api/config/clerk/inspect`, `/start`, `/callback`, `/sign-out` (operator or Owner) |

`GET /api/auth/me` returns
`{ authenticated, role, default_view, can_acquire, elevated, impersonating?, portal?, user?, second_factor? }`:

- `role` is `operator`, `owner`, `administrator` or `member`.
- `user` is the first-party user, when there is one.
- `default_view` comes from the caller's preferences.
- Portal users also get
  `second_factor: { required, totp, passkey_count, enrolled }`.

When the library does not answer in time to check a portal session (a
busy or still-starting server), `/api/auth/me` and the signed-in API routes
answer `503` with `Retry-After` and the slug `session_check_unavailable`,
not `401`. The request is still refused, but the web app waits and asks
again instead of showing the sign-in screen. A few handlers that check the
session again themselves, such as elevation and the session list, still
answer `401` in that case
([#726](https://github.com/Bookclerk/bookclerk/issues/726)).

## Client routes

The web app keeps the address bar in step with the screen:

| Path | Screen |
| --- | --- |
| `/` | The user's `default_view`; the address then changes to that path |
| `/discover` | Discover |
| `/library` | Library |
| `/wishlist` | Wishlist |
| `/accounts` | Accounts |
| `/settings` | Settings |
| `/invite` | Claim an invite (`?ticket=`) |
| `/clerk/approve` | Give a clerk a library card (see [Library cards](#library-cards)) |
| `/clerk/callback` | Finish this install's library card sign-in (see [Signing in with a library card](#signing-in-with-a-library-card)) |

`bookclerkd` serves `index.html` for these paths, so a hard refresh on
`/library` loads the app instead of a 404. Assets still come from
`ui/dist`, and unknown paths get a branded 404.

### Invite links

- Only `/invite?ticket=` claims an invite. On any other path, including
  `/discover`, the `ticket` is ignored and dropped from the address.
- A missing, invalid, expired or used ticket gets a branded **400** or
  **410** page instead of the form.
- The invite page is sent with `Cache-Control: no-store`.
- After a claim, the address becomes `/` (without the ticket), so the
  user's `default_view` applies.

## Start page

On `/`, the web app opens the user's **`default_view`**: `discover` (the
default), `wishlist`, `library` or `accounts`. A deep link such as
`/library` wins over it, and the header logo goes to the start page.

Change it with **Preferences** in the header, or:

```http
PATCH /api/preferences
Content-Type: application/json

{ "default_view": "library" }
```

It is stored per user (subject `user:{id}` for a linked user, `portal:{identity_id}` for an
unlinked identity, or `operator`).

## Appearance

The web app is designed in light; dark is an adaptation of the same ink,
teal, brick and parchment palette.

- The theme is `system` (the default), `light` or `dark`.
- **System** follows `prefers-color-scheme`. It uses light unless the OS
  explicitly asks for dark.
- An explicit **Light** stays light on a dark OS.
- The sign-in screen stores the choice in `localStorage`
  (`bookclerk-theme`). After sign-in it roams with
  `PATCH /api/preferences` `{ "theme": "dark" }`.
- Branded daemon pages (invite, 404) can't read preferences, so they
  follow `prefers-color-scheme` only.

## Screens

### Discover

- Shelves that scroll sideways, with catalog search across stores and
  autocomplete. Wishlist a title from a suggestion or a card.
- Live store prices load for visible cards only, in batches. `best`
  prefers the stores the viewer has linked.
- **On the wishlist** cards show small profile pictures of the people who
  want each title.
- There is no approval or triage step.

### Wishlist

- **Your wishes**: removing one removes your row and lowers or drops the
  title in the shared queue.
- **Global queue** (sidebar): one shared order for everyone, ranked by
  Discover taste with a strong boost for titles several people want. It is
  store-agnostic, and each row shows who wishlisted it.

### Library

The full library, in one scrolling list, for every signed-in role.
Operators also see scan and acquire. Connecting stores stays in
**Accounts**.

### Accounts

Link and revoke stores, and manage linked sign-in identities.

**Revoke** marks the account revoked (which stops its scans), signs it
out at the store (best effort), deletes its saved credentials, then
unlinks it. Books already in the library stay. If a step fails, the store
stays listed and the page shows the error, so **Revoke** can be tried
again.

### Settings → Account (everyone)

- **Profile**: display name, email and picture. The role is shown;
  numeric user ids are not. Name and email save on blur or Enter, and
  Escape cancels.
- **Security**: password, named passkeys, authenticator app, linked
  identity providers, and Owner elevation.
- **Sessions**: browsers and library cards, each with **Revoke**. A
  library card shows the name its clerk gave itself.

Profile edits are self-service. Security and profile edits are hidden or
disabled while impersonating.

**Profile pictures.** Clicking the picture offers a monogram, Gravatar
(when there is an email), SSO pictures or an upload. Automatic choice
prefers an upload, then the last SSO picture, then Gravatar
(`https://www.gravatar.com/avatar/{sha256}?d=404`), then the monogram.
Gravatar and SSO pictures are downloaded by Bookclerk and served from its
own address, so the browser never contacts Gravatar or the provider (see
[Other images](integrations.md#other-images)).

- Uploads are JPEG, PNG or WebP, saved as
  `$BOOKCLERK_FILES_DIR/avatars/{id}.{ext}`.
- `users.avatar_source` stores an explicit choice: `auto`, `monogram`,
  `gravatar`, `upload` or `sso:{id}`.

### Settings → User Management

The operator adds the first Owner. The operator, Owners and Administrators
create users with an invite link, change role and status, send a new invite, and
reset passwords.

Who can grant which role:

- An Administrator adds Members only.
- An Owner adds Members, Hosts and Administrators.
- The operator, or an elevated Owner, can grant any role.

Each row shows presence:

| Mark | Meaning |
| --- | --- |
| Dashed grey ring | Never signed in |
| Solid grey | Seen before; session expired |
| Brick (orange) | Signed in, idle |
| Teal | Active in the last 5 minutes or so |
| Logo waveform | Listening progress changed in that time, usually from a player integration |
| Brick X | Account disabled |

`users.last_seen_at` survives logout.

### Settings → Sign-in

For the operator and Owners. In order: the second-factor policy, SSO into
Bookclerk, and Bookclerk as an identity provider (with copyable discovery
URLs and the client cards). Administrators can't change these.

### Impersonating

The operator or an elevated Owner can act as another user. A brick
banner at the top names them (`Impersonating Alex as Owner`), with
**Stop**.

- The Settings tabs follow the impersonated role: User Management for
  Owners and Administrators, Sign-in for Owners.
- **Server** and **Plugins** stay operator-only until impersonation ends.
- **Stop** signs an elevated Owner back in as themselves, so ending
  elevation afterwards leaves them signed in rather than signed out.

### Settings → Server and Plugins

For the operator, or an elevated Owner: listen addresses, sign-in,
auto-acquire, isolation and jail limits, and turning plugins on.

Turning on a plugin shows a branded consent dialog. You can widen or
narrow its grants, within the host's workerd and disk caps and its domain
allowlists.

### Settings → Library

For the operator, or an elevated Owner: the libraries this server is a
branch of.

- **Join a library**: the address, the fingerprint for a self-signed
  library, then a sign-in on that library. Or choose **I have an invite**.
- **Leave library**.
- Each library plugin that asks to run here, with a deny checkbox for each
  domain and binding, a switch to deny network, **Approve** (for the
  request shown, archive included) and **Revoke**. A refused approval
  shows the daemon's reason.

### Settings → Branches

For the operator and Owners: **Let new branches join**, the address to
join, and the fingerprint a joining operator checks.

- Branch invites come from **Add a branch** in User Management (and each
  user's own menu), and are shown once.
- On a fresh install, the operator first sees a welcome screen: **Start a
  new library** or **Join a library**. In the desktop app it opens on
  **Sign in to your library** with a library card instead. An install
  that holds a library card is no longer fresh.

See [Libraries and branches](branches.md).

## Live updates

The web app keeps one WebSocket open: `GET /api/live`, subprotocol
`bookclerk-live.v1`. It names topics whose data changed: `library`,
`jobs`, `deliveries`, `nodes`, `config`, `consents`, `libraries`,
`users` and `discover`. Status, Library, Discover, Settings → Nodes,
User Management and Settings → Library re-read their data when told.

- The socket carries topic names only.
- It needs an exact `Origin` match and a signed-in session, and is
  scoped to the caller's role.
- While it is down, screens poll on their old timers, and the client
  reconnects with backoff from 1 to 30 seconds.
- The Vite dev server proxies it (`/api/live` with `ws: true`).

Details:
[Node API and the live socket](../internals/node-api.md#the-live-socket-apilive).

## Next steps

- [Security checklist](security.md): sign-in settings for a public
  server.
- [Managing plugins](plugins.md): turning plugins on and approving them.
- A future plugin browser is sketched in
  [Plugin registry](../plugins/registry.md). Today, plugins are managed in
  Settings and the CLI; see [Plugins](../plugins/README.md).
