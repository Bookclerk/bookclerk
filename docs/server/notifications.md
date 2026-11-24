# Notifications

Bookclerk can tell people when something happens: a sync that failed, a
book that arrived, a recommendation. The host decides who hears about
what; notifier plugins only deliver one finished message to one place.
The design is in the [notifications ADR](../adr/notifications.md).

> [!NOTE]
> Routing, the delivery log, the inbox, the web app's bell with browser
> and desktop notifications, the web API, the
> [webhook notifier](#the-webhook-notifier), the
> [email notifier](#the-email-notifier) and
> [account mail](#account-mail) (emailed invites, verified addresses and
> [password reset](#password-reset), with
> [recovery codes](sign-in.md#recovery-codes) in place of a lost second
> factor) are built. Recovery by administrator approval or after a
> waiting period, and the chat, paging and push notifiers, come later.

## How a notification travels

1. Something raises a notification: the host itself, or a plugin through
   its [`NOTIFY` binding](../plugins/abi.md#raising-notifications).
2. The host stores it, renders it once, and picks its destinations by the
   rules below. A patron notification also lands in that patron's inbox,
   and an operational one routed to **In app** lands in every active
   Owner's and Administrator's inbox.
3. Each destination gets a row in the delivery log and one `notify_send`
   job, which calls the notifier instance's `send`.
4. The notifier answers delivered, retry, or failed, and the log records
   it.

Every notification has a type, a severity (`info`, `warning` or
`critical`) and an audience:

| Audience | Who hears about it | Who decides where it goes |
| --- | --- | --- |
| `operational` | The people who run the library | Administrators, per type |
| `patron` | One patron | Administrators offer notifiers; each patron chooses |
| `account` | One person's sign-in mail | Not routed yet |

Host types have plain ids such as `test`. A plugin's types are named
`<plugin id>:<type id>`, so two plugins never collide.

## Channels and routes (administrators)

A **channel** is a library-wide destination: one notifier instance,
whose own settings say where messages go (a Slack webhook, a PagerDuty
service). Owners and Administrators manage channels under
`/api/notifications/channels`, and can rename, resume or delete
one.

A **route** sends one operational type to a channel, from a minimum
severity up. `PUT /api/notifications/routes/{type}` replaces a type's
routes; a type with no routes goes nowhere. An empty replacement also
clears a type the catalog no longer lists, or one a plugin upgrade moved
to the patron audience, so rules left from before can be removed (offers
work the same way). Offers for a type that is now operational make no
notifier eligible for patrons. `GET
/api/notifications/catalog` lists every known type and every notifier
instance, with the address fields each one asks patrons for.

**In app** is a built-in route target with the reserved destination id
`in-app`. It runs no plugin: an operational notification at or above its
minimum severity goes into the inbox of every active Owner and
Administrator, read when the notification is stored (a disabled account,
or someone who is no longer an Administrator, gets nothing). Route it like
a channel; `GET /api/notifications/routes` lists it first in its type's
routes.

To check a channel, `POST /api/notifications/channels/{id}/test` sends it
the host's `test` notification. A paused channel answers `409`: resume it
first.

## Offers and choices (patrons)

Administrators **offer** notifiers to patrons per patron type, with
`PUT /api/notifications/offers/{type}`. Only instances whose notifier
sends to personal addresses can be offered. Each offer says whether it is
on by default.

A patron adds their own address on a notifier offered for at least one
type with `POST /api/me/notifications/destinations`, and keeps at most 32
addresses. The address is a flat object
of strings, checked against the notifier's declared address fields and
then by the notifier itself, which may normalise it. The notifier must be
running to check it (`409` otherwise). The stored address is at most
1 KiB of JSON, and no more than the library database's cell limit, or it
is refused with `400`.

For each patron type, a patron's notifications go to:

- the destinations the patron chose with
  `PUT /api/me/notifications/choices/{type}`, each on a notifier offered
  for that type;
- nowhere but the inbox, when the patron turned the type off;
- otherwise, their addresses on the notifiers offered on by default.

A choice with no destinations (or `destination_ids` left out) sends
nothing outside the inbox, even with `enabled: true`.
`DELETE /api/me/notifications/choices/{type}` returns to the default. A
patron reads their settings with `GET /api/me/notifications`, and their
inbox with `GET /api/me/notifications/inbox`; `POST
/api/me/notifications/inbox/read` marks entries read.

## The bell, browser and desktop notifications

Every signed-in library user has a bell in the web app's top bar. It shows
their newest inbox entries and how many are unread; opening an entry marks
it read and goes to the page it links to, and **Mark all read** clears the
rest.

The bell follows the inbox and nothing else. The web app hears about new
entries on the `notifications` [live topic](sign-in.md#live-updates),
which changes whenever an entry arrives in any inbox or someone reads one,
and then re-reads `GET /api/me/notifications/inbox`. It also re-reads
every minute while the live connection is down.

A person can also turn on alerts for one device from the bell:

- **In a browser**, **Show browser notifications on this device** asks for
  the browser's permission. New unread entries then show as browser
  notifications while the app is open in a tab that does not have focus;
  clicking one focuses the tab and opens the entry's page. A browser that
  blocked the site says so, and only its site settings can undo that.
- **In the desktop window**, **Show system notifications on this device**
  posts each new unread entry as a native notification (D-Bus on Linux,
  the notification center on macOS, a toast on Windows) while the window
  is not focused. See [the desktop window](../contributing/ui/README.md#the-desktop-window).

The choice is remembered per device, in the browser's storage. Entries
that were already in the inbox when the page loaded are not announced
again, and several open tabs show one alert per entry, not one each.

A plugin can only raise a patron notification for a patron who holds the
store account the plugin is working for, so a store plugin cannot message
people outside its own account.

## The webhook notifier

The first-party `webhook` plugin posts each notification as JSON to a URL,
signed per the [Standard Webhooks](https://www.standardwebhooks.com)
scheme, so any service that can check a signature can receive it.

To set up a channel:

1. Make a signing secret: `whsec_` followed by 24 to 64 random bytes in
   base64, for example `whsec_$(openssl rand -base64 32)`. Give the same
   secret to the receiver.
2. Install the plugin and approve its network grant. It asks for any
   public website (`*`); deny that and approve the receivers' hosts
   instead if you prefer. A receiver on a private or loopback address
   also needs that address's CIDR in the grant.
3. Set the instance's `url` setting to the receiver's URL (`https://`, or
   `http://` on a network you trust) and its `signing_secret` secret to
   the secret.
4. Add the instance as a channel and send it the `test` notification.

Patrons can use the same plugin for their own endpoints once an
administrator offers it: a personal address holds a `url` and a `secret`,
both sealed, and the patron sees only the URL's host.

Each delivery is one `POST` with these headers:

| Header | Value |
| --- | --- |
| `webhook-id` | `msg_<notification id>_<destination id>`: the same on every retry to one destination, so a receiver can drop duplicates |
| `webhook-timestamp` | When it was sent, in Unix seconds |
| `webhook-signature` | `v1,` and the base64 HMAC-SHA256 of `<id>.<timestamp>.<body>` under the secret's key |

The body is `{ "type", "timestamp", "data" }`. `type` is the notification
type and `data` carries the message: `id`, `type`, `severity`, `title`,
`text`, `fields`, `alertAction`, and, when present, `markdown`, `html`,
`link`, `groupKey` and `alertKey`.

The receiver's answer decides what happens next:

- `2xx`: delivered.
- `429`: retried, after its `Retry-After` when it sent one.
- `408`, `425`, `5xx`, a timeout after 20 seconds, or no connection:
  retried on the job's backoff.
- A redirect: failed. The plugin never follows one, so a signed body only
  reaches the URL you set; set the receiver's final URL.
- Any other status: failed for this message.

## The email notifier

The first-party `email` plugin sends each notification as an email
through a mail server you already use (your provider's SMTP submission
server, or your own). Each message has a plain text part and a branded
HTML part, both quoted-printable so they stay readable in any mail
client.

The HTML part is laid out with [MJML](https://mjml.io), compiled inside
the plugin, so it renders the same in mail clients that strip styles
from the document head. It shows the Bookclerk mark and name, then a card
with the From name, the title, the body and the link as a button labelled
"Open" followed by the From name ("Open your library" when the From name
is `Bookclerk`). The mark travels inside the message as an inline image,
so nothing is fetched when the mail is opened. The footer links your
library's home page (from its public origin) and bookclerk.app. Invites
also carry a short introduction to Bookclerk, for recipients who have not
heard of it. When the From name is set and isn't just `Bookclerk`, an
invite's subject names the library by it ("Alex has invited you to
Riverside Library"); otherwise the subject stays "Alex has invited you
to a Bookclerk library". When the HTML a notification brings cannot be
laid out, the plugin sends a plain HTML document instead.

To set up a channel:

1. Install the plugin. It asks for no hosts up front.
2. Set the instance's `server` setting to the mail server, as
   `host` or `host:port` (port 587 when you leave it out), and approve
   the network grant that setting asks for.
3. Pick `security`:
   - `starttls` (the default): connect in the clear on 587 and upgrade
     to TLS before anything else is sent. The plugin stops if the
     server does not offer the upgrade.
   - `tls`: TLS from the first byte, usually on port 465.
   - `none`: no encryption, for a relay on a network you trust. The
     plugin never signs in over it.
4. Set `username` and the `password` secret if the server needs a
   sign-in, `from_address` (the sender), and optionally `from_name`
   (default `Bookclerk`).
5. Set `to` to one address, or up to 10 separated by commas, for the
   channel's messages.
6. Add the instance as a channel and send it the `test` notification.

Patrons can use the same plugin for their own address once an
administrator offers it; a personal address holds one `email`.

Until `server` is set, the plugin reports itself healthy with "no mail
server set yet" and every send fails. Its health check reads the settings
only and never connects to the server.

The plugin checks the server's certificate against the usual public
roots, signs in only after TLS (`AUTH PLAIN`, or `AUTH LOGIN` when that is
all the server offers), and marks every message `Auto-Submitted:
auto-generated` so mail clients do not answer it.

### Checking deliverability

An email instance's settings page has a **Check deliverability** button.
It sends the `test` notification, built as a real one is, through up to
three checkers and turns what they find into a list of recommendations,
most urgent first:

- **Postmark spam check** (`spamcheck.postmarkapp.com`) scores the message
  with SpamAssassin. Rules about the sending domain are left to the DNS
  check, since an unsent message has no signature yet.
- **DNS records** (Cloudflare's DNS-over-HTTPS resolver,
  `cloudflare-dns.com`) looks up the SPF record of the `from_address`
  domain and its DMARC record at `_dmarc`.
- **mail-tester.com** (`www.mail-tester.com`) is used only when you set
  `mail_tester_account` to your mail-tester account name. The plugin
  sends a real test message through your mail server to a mail-tester
  address, waits up to a minute for the score, and links the full report.
  It uses one of the account's test credits.

Each checker's host is a network grant you approve in the plugin's network
settings; a checker whose grant is not approved is shown as not run. The
check uses the saved settings and takes at most about 100 seconds; when
mail-tester has not scored the message by then, the report links its
page so you can read the score there. The instance handles one call at a
time, so sends and patrons' address checks wait while a check runs. An
invalid `mail_tester_account` never stops mail: only the check reports
it. Only administrators can run it.

### When a send fails

What the server answers decides what happens next:

- Accepted: delivered.
- A `4xx` reply, a timeout after 30 seconds, or no connection: retried
  on the job's backoff.
- A refused recipient (`550`, `551` or `553`): that address does not
  exist; the message fails for this destination.
- A refused sign-in: the message fails; check `username` and `password`.
- The network grant for `server` not approved yet: the message fails
  until an administrator approves it.
- Any other `5xx`: the message fails.

## Account mail

Account mail is what the library sends about a patron's own account:
invites, links that verify an email address, password-reset links and
the notice that a password was changed. It is never routed and
patrons cannot turn it off. It goes through one notifier instance an
administrator chooses, and only plugins that say they can carry it (the
[email notifier](#the-email-notifier) does) can be chosen.

To turn it on:

1. Set up the email notifier as above. Its `server`, sign-in and
   `from_address` settings are all account mail needs; `to` is only for
   channels.
2. Set the library's public address, `integrations.public_origin`. Every
   link in account mail is built from it, never from the address a
   request happened to use.
3. Open **Settings → Notifications → Account mail**, pick the instance
   and select **Send a test** to mail your own address. The page says
   whether account mail is ready and when a test last arrived.

Once it is ready:

- **Invites go by email.** Creating a user with an email address mails
  them the invite instead of handing you a link. **Email invite** on a
  user sends a new one. An invite works once and for 7 days, and only
  the newest one works: each send voids the user's other unused invites.
  Using an emailed invite verifies that address. The invite names the
  administrator who created the user or sent it, by display name (or
  sign-in name); one sent with the operator token names no one. The
  notification carries that name in its title and in an `Invited by`
  field. The email notifier drops the field from the message and names
  the inviter in the subject instead, along with the library's From name
  when that name is set and isn't just `Bookclerk`. Invites go out as notification
  type `account_invite` and the rest of account mail as `account_mail`,
  so a notifier can word them differently.
- **Addresses are verified.** When a patron sets or changes their address,
  or an administrator changes it, the library mails a link that verifies
  it. The link works once, for 24 hours, and only while the patron still
  has that address; changing the address marks it unverified again. A
  patron who lost the message can ask for a new one from their profile,
  once every five minutes; requests that overlap record one message,
  and the rest are answered `429` with `Retry-After`. Opening the link
  needs no sign-in.

Each message's link is made by its send job at the moment it is sent.
Only a hash of the token is stored, the link is written nowhere but the
message, and the message is marked sensitive so notifiers never log it.
A send that is retried makes a new link and voids the last.

Account mail retries like any other send (see below). A message whose
user was deleted or disabled, whose address changed before it went out
(checked again in the same write that makes its link), whose notifier no
longer opts into account mail, or that has no public address to link to
is dropped. Records of sent
account mail, without their links, are kept for 30 days.

The web API:

- `GET` and `PUT /api/notifications/account-mail` read and choose the
  instance (`{"plugin_instance_id": "…"}`, or `null` to turn account mail
  off), with whether it is ready, why not, and when a test last arrived.
- `POST /api/notifications/account-mail/test` mails the caller's own
  address.
- `POST /api/users/{id}/invite-email` sends a user a new invite.
- `POST /api/auth/profile/email/verification` sends the caller a new verification
  link.
- `POST /api/auth/verify-email` with `{"token": "…"}` uses a link.

### Password reset

A patron who forgot their password can ask for a reset link from the
sign-in page with **Forgot your password?**. The sign-in page offers it
only once account mail is ready and a test message through the chosen
notifier arrived, so a library whose mail does not work never promises
a link.

1. The patron enters their address. The answer is the same whether or
   not an account has it, and the work runs in a job, so neither the
   answer nor its timing says whether the address is known.
2. When an enabled account has that address **verified**, the library
   mails a link to it, at most one every five minutes per account. An
   unverified address gets nothing, so verify addresses before patrons
   need a reset.
3. The link works once, for 30 minutes, and only while the account still
   has that address verified. A new link voids the last. The password
   does not change until the link is used, so asking for links cannot
   lock anyone out.
4. Opening the link asks for a new password. An account with an
   authenticator app also asks for its current code; one with passkeys
   and no authenticator asks for a passkey. Either account can enter one
   of its [recovery codes](sign-in.md#recovery-codes) instead. A wrong
   code does not use the link up.
5. Saving sets the password, signs every device out (browser and
   elevated sessions, and revokes OpenID refresh tokens) and mails a
   notice that the password was changed. The change is recorded in the
   security audit log as `password_reset_self`, in the same atomic write.
   With a recovery code, that write also uses the code up and removes
   the account's authenticator app, passkeys and other codes, since the
   patron lost them, and the audit row is
   `password_reset_recovery_code`. The patron sets up a new second
   factor after signing in.

Links that do not work and wrong codes count toward a per-client limit,
the same one failed sign-ins use, so repeated tries are answered `429`
with `Retry-After`. Asking for links has a limit of its own, so asking
several times cannot lock a patron out of the link that arrives. When
the notification queue is full, a request is answered `503` with
`Retry-After` and nothing is queued; the answer is the same for every
address.

Saving the new password, signing every device out and using the link up
happen in one atomic write that repeats the second-factor check, so a
write that does not commit leaves the link working and the password
unchanged, and an authenticator or passkey added after the link was
opened makes saving fail without using the link up. When the database's
reply is lost, Bookclerk sends the same write again and then compares
the stored password with the new one, so a write that did commit is
still reported as a success. If the retry or that check fails too, the
outcome is unknown: the patron sees an error, and the
new password may already be in effect. The password-changed notice is queued
after that write, on a best-effort basis: if it cannot be queued, the
reset, the sign-outs and the audit row still stand, and the failure is
logged.

An administrator can still reset a password from **Settings → Users**:
**Reset password** signs the user out and shows a link to send them, and
**Email the link** mails them a new invite instead when they have an
address.

The web API (no sign-in needed; the link is the proof):

- `GET /api/auth/password-reset` says whether reset is offered
  (`{"available": true}`).
- `POST /api/auth/password-reset` with `{"email": "…"}` asks for a link;
  it answers `202`, `409` with `reset_unavailable` when reset is off, or
  `503` with `busy` when the notification queue is full.
- `POST /api/auth/password-reset/check` with `{"token": "…"}` checks a
  link without using it and says which second factor it needs
  (`second_factor`: `none`, `totp` or `passkey`) and whether a recovery
  code may stand in for it (`recovery_code`).
- `POST /api/auth/password-reset/passkey` with `{"token": "…"}` begins
  the passkey step.
- `POST /api/auth/password-reset/finish` with `token`, `password` and
  one of `totp_code`, `passkey` (`challenge_id` and `credential`) or
  `recovery_code` sets the password.

## Retries, pausing and the delivery log

- A send runs on a node that holds the library database, so deploy each
  notifier instance on at least one such node. A send job for an instance
  that runs only elsewhere, or is deployed but not running, waits in the
  queue for up to an hour; then the job is cancelled and the delivery
  fails, counting toward a pause (or is dropped, if its destination was
  paused meanwhile). A send that waits that long only because other work
  is ahead of it, while its notifier is running, is left in the queue and
  counts nothing.
- A notifier that does not answer within 30 seconds is retried like one
  that asked to.

- A notifier that asks to retry is tried again after the wait it gave,
  or after a backoff that starts at 30 seconds and doubles up to an hour.
  A delivery makes at most 10 attempts.
- A notifier that refuses a message for good fails that delivery, and so
  does running out of attempts. After 5 failed deliveries in a row its
  destination is **paused**: nothing more is sent
  to it until it is resumed: a channel by an administrator
  (`POST /api/notifications/channels/{id}/resume`), a patron's address by
  that patron (`POST /api/me/notifications/destinations/{id}/resume`). A
  delivery clears the count.
- A delivery whose destination is paused or removed, or whose notifier
  instance is gone, is **dropped**, not sent. So is one whose send job was
  cancelled, and one the host itself cannot render into a sendable
  message; neither counts toward a pause.
- A sweep every minute schedules any delivery left without a job, for
  example when the job queue was full.

`GET /api/notifications/deliveries` pages the delivery log, newest first,
optionally for one destination.

## Limits and retention

- A plugin may raise at most 30 notifications a minute per session (an
  instance has one session per store account it works for); more answer
  `rate_limited`, and refused raises count too.
- A type has at most 32 routes, offers or chosen destinations.
- A raised notification's plain-text body is its fields, one `name: value`
  line each, cut with `…` at 16 KiB; the fields themselves are kept whole.
- Notifications, their delivery log and inbox rows are deleted after 30
  days by the record retention pass (see
  [Jobs and scheduling](../internals/jobs.md)). So are records of sent
  account mail, and expired verification and password-reset links.
