# ADR: Notifications are routed by the host and sent by plugins

- **Status:** Accepted; not yet built
- **Date:** 2026-11-20
- **Related:** [Event delivery model](event-delivery-model.md),
  [First-party identity](first-party-identity.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md)

## Context

Bookclerk has no way to tell anyone that something happened. Three kinds
of message are wanted:

- **Account mail.** Invites, password resets and email verification.
  Today an invite is a 7-day claim ticket that an administrator copies by
  hand. Password resets are administrator-only, clear the password at
  once and hand back a new claim ticket. A user's email address is an
  optional contact field that is never verified. There are no recovery
  codes; only the operator can clear a lost second factor.
- **Operational alerts** for administrators: a failed job, a store
  sign-in that expired, an unhealthy plugin, an offline node, low disk.
- **Patron notifications** that each patron sets up for themselves:
  store recommendations and price drops from Discover, new books by
  authors or series they follow, fulfilled requests and listening
  suggestions.

Administrators must decide which kinds of notification reach which
services, and patrons must control their own. Delivery has to work over
email and over chat and push services (Discord, Slack, Telegram,
PagerDuty and others), each with its own API.

The [event delivery model](event-delivery-model.md) separates events (a
fact every subscriber receives) from jobs (a command one handler claims).
Plugins run jailed in workerd with only the hosts they declared and the
operator approved. A workerd plugin already has hostname-filtered
`fetch()`, and `connect()` with `secureTransport: "starttls"` under a
`tcp` grant, which is enough for every HTTP API and for SMTP without a
third-party binary.

## Decision

1. **Routing in the host, sending in plugins.** A new `notifier` plugin
   family (entrypoint class `Notifier`) only sends one message to one
   destination. A host notification router subscribes to domain events,
   applies the administrators' routing rules and each patron's
   preferences, renders the message, and enqueues one `notify.send` job
   per recipient and destination. A notifier instance claims the job;
   retries, backoff, give-up and the delivery log belong to the job.
   Notifier plugins do not subscribe to domain events themselves, so
   policy lives in one place and patron data reaches only the instance
   chosen for it.
2. **Notification types and audiences.** The host keeps a registry of
   notification types, each with an audience:
   - *account* (invite, password reset, verify email, password changed,
     new sign-in, recovery started): cannot be turned off and is reserved
     to the host. Anything carrying a link goes only through the email
     instance the administrators mark for account mail. The security
     notices (password changed, new sign-in, recovery started) also go,
     without any link, to the inbox and the patron's other verified
     personal destinations, telling them to check their email or sign in;
   - *operational*: administrators route each type to library channels
     (a Slack channel, a Discord webhook, a PagerDuty service, email to
     administrators) with a minimum severity, and alerts may carry a
     trigger and resolve lifecycle keyed by an alert key;
   - *patron*: administrators choose which instances patrons may use for
     each type and its default; each patron turns it on, picks
     destinations, narrows it (stores, price, language, followed authors
     and series) and chooses immediate delivery or a daily digest, with
     quiet hours in their own time zone.
3. **The plugin interface.** `plugin.capnp` and every SDK gain, with new
   ordinals, a `notifier` entrypoint: `send(message, destination)`
   answering delivered, retry (with an optional delay, honouring
   `Retry-After`) or failed (permanent or not); `validateDestination`;
   optional `beginLink` and `completeLink` for flows such as a Telegram
   `/start` code; and `health`. The manifest's `[notifier]` table says
   which destination kinds it supports (`channel`, `personal`), whether
   it can carry account mail, which formats it renders and the address
   fields a personal destination needs. The message is rendered by the
   host: type, severity, title, plain text, optional markdown and
   minimal HTML, a link built only from `integrations.public_origin`
   (never a request's `Host`), fields, a grouping key and, for alerts, an
   action and alert key. Fields that hold secrets are marked so the SDKs
   never log them.
4. **Plugins raise their own types.** Any plugin may declare
   `[[notifications.types]]` (id, audience, severity, title) and raise
   them through an `env.NOTIFY.raise` binding. A plugin may declare only
   operational and patron types; the account audience is the host's. They
   appear in the routing
   matrix beside the host's types, any notifier can carry them, and the
   raising plugin never learns where they went. A raise is handed to the
   router, which turns it into jobs. A patron-audience raise must name a
   patron inside the invocation's account scope, and raises are
   rate-limited per instance. Nothing stops a plugin with a network grant
   from posting to a chat service itself, so the manifest check flags a
   plugin that is not a notifier but asks for known notification hosts,
   and its consent screen says it can send messages outside Bookclerk.
   Patrons' destinations are never handed to other plugins.
5. **Account mail is never an event.** Its links are secrets, and the
   outbox is readable by consenting plugins and kept for the events
   window. The host enqueues account-mail send jobs directly with the
   link sealed, and erases it when the job finishes or expires. Tokens
   and links appear in no log, event or audit row.
6. **Invites and verified email.** Creating a user with an email address
   sends the invite by account mail; the 7-day ticket is unchanged and a
   resend voids the old one. Claiming an emailed invite verifies that
   address. Adding or changing an address sends a 24-hour, single-use
   verification link. Only verified addresses receive password resets.
7. **Self-service password reset.** `POST /api/auth/password-reset`
   always answers `202` with the same body and does its work in a job, so
   neither the answer nor its timing reveals whether an account exists.
   The token is 32 random bytes, stored hashed, single use and valid for
   30 minutes; a new request voids the last. The password stays until the
   link is used, so requesting resets cannot lock anyone out. Using the
   link asks for a second factor when the account has one, then sets the
   password, revokes every session and sends a password-changed notice.
   Requests are throttled per address and per client, which depends on
   fixing how the sign-in throttle reads `X-Forwarded-For` first. The flow
   stays off until an account-mail instance is set and has sent a test
   message and `integrations.public_origin` is set. Administrator resets
   keep working and can also email the link.
8. **Recovering a lost second factor.** A patron who forgot their
   password has often lost their authenticator too. Recovery starts only
   from a password-reset link (decision 7), so it needs control of a
   verified email address, and that address is the channel that can
   cancel it. Recovery is a ladder:
   a passkey sign-in (passkeys usually sync); new single-use recovery
   codes, 10 issued when a second factor is enrolled, stored with Argon2id
   and regenerable; administrator approval of a recovery request, under
   the same rules as provisioning; and, when no administrator approves or
   for the only Owner, delayed self-recovery that completes after 3 days
   (Members and Hosts) or 7 days (Administrators and Owners), adjustable
   or switchable off by Owners. A recovery notice goes by account mail to
   the verified address when it starts and daily after, each with a
   single-use link that cancels it without signing in; the link-free
   security notice also reaches the inbox and the patron's other verified
   destinations, and any successful sign-in cancels it. An account
   without a verified email address cannot start delayed recovery: it
   recovers through an administrator or the operator. Other Owners hear of recoveries of administrator
   accounts. The operator can recover a user from the host's command
   line. Every step is throttled, answers the same whether or not the
   account exists, and leaves an audit row without tokens.
9. **An in-app inbox is always on.** Patron notifications always reach a
   host-owned inbox in the web app and desktop window, so a patron who
   sets up nothing still sees them.
10. **Privacy of library channels.** A patron's own listening, wishlist
    and recommendation items never go to a library channel unless that
    patron opts in; shared channels get library-level facts only.
11. **First providers.** All are TypeScript workerd plugins: email over
    SMTP (`connect()` with STARTTLS on 587 or TLS on 465), Discord, Slack
    (which also covers Mattermost and Rocket.Chat), Telegram, PagerDuty
    Events API v2, ntfy, Web Push (VAPID key as a sealed secret) and a
    generic webhook signed per the Standard Webhooks scheme. Later, on
    the same interface: email APIs (Postmark, Resend, Amazon SES,
    Mailgun, SendGrid), Pushover, Gotify, Matrix, Home Assistant, an
    Apprise API server, Microsoft Teams through a Workflows webhook, and
    mobile push once phone clients exist. Signal (no official API; every
    bridge needs a third-party binary), WhatsApp (business verification
    and per-message fees) and SMS are not planned.

## Consequences

- `plugin.capnp` gains the `notifier` entrypoint, its structs and limits,
  and the `NOTIFY` binding, with the TypeScript, Python and Rust SDKs in
  the same pull request.
- New tables for destinations, routing rules, patron preferences, the
  delivery log, the inbox, reset and verification tokens, recovery codes
  and recovery requests go through the SQL-v1 contract.
- New untrusted-input boundaries, each with a fuzz target, seed corpus
  and replay test: notifier replies (`notifier_reply`), destination
  address documents (`notify_destination`), plugin raises
  (`notify_raise`), and the new web API bodies.
- Notifiers sleep behind the on-demand proxy like other plugins: a
  `notify.send` job wakes one, and a Telegram link code is polled only
  while one is pending.
- Patron notifications need new domain event types (Discover
  recommendations and price drops, new books, fulfilled requests,
  listening suggestions); the ones that depend on the library data model
  wait for it.
- Work lands in this order: the core (entrypoint, SDKs, router, jobs,
  delivery log, inbox, APIs and the generic webhook plugin); account mail
  (SMTP plugin, emailed invites, verification, self-service reset and
  recovery); the web app settings and inbox; the other providers; then
  the producers of operational and patron events, with digests. Each
  updates the pages under `docs/` it changes.
