# Notifications

Bookclerk can tell people when something happens: a sync that failed, a
book that arrived, a recommendation. The host decides who hears about
what; notifier plugins only deliver one finished message to one place.
The design is in the [notifications ADR](../adr/notifications.md).

> [!NOTE]
> Routing, the delivery log, the inbox and the web API are built. No
> notifier plugin ships yet, and account mail (invites and password
> resets) comes later.

## How a notification travels

1. Something raises a notification: the host itself, or a plugin through
   its [`NOTIFY` binding](../plugins/abi.md#raising-notifications).
2. The host stores it, renders it once, and picks its destinations by the
   rules below. A patron notification also lands in that patron's inbox.
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

A plugin can only raise a patron notification for a patron who holds the
store account the plugin is working for, so a store plugin cannot message
people outside its own account.

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
  [Jobs and scheduling](../internals/jobs.md)).
