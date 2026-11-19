# ADR: Patron scopes and linked sign-in

- **Status:** Accepted
- **Date:** 2026-11-17
- **Related:** [First-party identity](first-party-identity.md),
  [Plugin-provided OIDC clients](plugin-oidc-clients.md),
  [Metadata claims and file sync](metadata-claims-and-file-sync.md)

## Context

Every Member sees the whole library. A household with children, or a
library shared with friends, has no way to show one person only part of
it.

Audiobookshelf already solves this, and many people run it beside
Bookclerk. Each Audiobookshelf user has a type (`root`, `admin`, `user` or
`guest`) and, below admin, a set of permissions: which libraries they can
open, an allow or deny list of tags, whether they see explicit content,
and whether they can download, upload, edit or delete.

The two servers can be linked in either direction:

- **Bookclerk relies on Audiobookshelf.** The Audiobookshelf plugin signs
  people in to Bookclerk with their Audiobookshelf credentials, and mints
  claim tickets for new Audiobookshelf users.
- **Audiobookshelf relies on Bookclerk.** Audiobookshelf's OpenID login
  uses Bookclerk as its provider, through the plugin's `audiobookshelf`
  client.

Today neither direction carries permissions or suspension. Any
Audiobookshelf user, a guest or a child restricted to one library
included, can claim a ticket that opens the whole Bookclerk library. A
patron disabled in Bookclerk stays signed in to Audiobookshelf, because
Audiobookshelf's own session outlives the provider.

Audiobookshelf's OpenID login, as of its current source, already reads
two claims when they are configured: a group claim that sets the user's
type, and an advanced permissions claim (`allowedLibraries`,
`allowedTags`, `tagsAreDenylist`, `canAccessExplicitContent`,
`canDownload`, `canUpload`, `canUpdate`, `canDelete`,
`canCreateEReader`). It applies them at every login. It ignores the
permissions claim for `admin` and `root` users, refuses to change a
`root` user's type, and checks `isActive` on every token.

## Decision

### Patron scopes

A Member has a **scope**: which part of the library they see and what
they may do with it. Owners and Administrators are never scoped.

| Field | Meaning |
| --- | --- |
| Rules | Which books are visible: a list of rules, each a field, a value, and include or exclude |
| Explicit content | Whether editions marked explicit are visible. Editions gain an `explicit` field, filled from storefront metadata and sidecars. |
| Member controls | What the Member may do, one switch each (see [Member controls](#member-controls)) |

A rule's field is one of collection, tag, genre, series, author or
narrator, and it matches one value of that field: "genre Kids", "series
Discworld", "author X". A collection is a named part of the library,
defined by storage key prefixes (for example `kids/`). A tag is a
free-form label an Administrator sets on a work, or one read from a
sidecar file. With no include rules every book passes; with any, a book
must match at least one. An exclude rule always wins over an include.

Collections cover files. A book held only through a store account has
no storage key until it is downloaded, so it belongs to no collection. A
collection-scoped Member sees the downloaded books inside their
collections. A Member always sees, and can open, every book they own
through their own store accounts, whatever their scope says.

Rules on metadata fields and the explicit content rule also filter books the library does
not hold, by their metadata: Discover shelves and storefront search
results show a scoped Member only what their scope allows.

The scope is enforced by one filter in the host's library queries, not by
a check per screen. It applies to the Library, search, facets and book
pages (covers included), Discover and the Wishlist, file download and
streaming, library cards and the clerk socket, and listening progress.
It runs on the library that holds the books, so a clerk or a window only
ever learns of the books the patron may see.

Existing patrons are unrestricted when scopes ship. New patrons get the
library's default scope, which an Administrator sets; out of the box it
has no rules and allows explicit content, because
explicit flags will be incomplete at first.

### Member controls

Beside the rules, six switches say what a Member may do. Each is on out
of the box.

| Control | Turning it off stops |
| --- | --- |
| Link store accounts | Connecting a store account, signing in to one again, and bringing accounts from another library. Accounts already connected keep working, and an Administrator can still remove them. |
| Download | Taking copies of the library's files: downloads to a branch or clerk, streaming, and library-card sync |
| Edit metadata | Changing book metadata |
| Upload | Adding books by upload |
| Request books | Asking a branch for a book it listed (interlibrary requests) |
| See prices | Prices and store links in Discover, catalog search and the Wishlist; the library drops them from its answers, so the patron's browser never receives them |

The library has a default for every switch, which an Administrator
sets, and an Administrator can set single switches for one Member. A
switch set for one Member overrides only that switch; the others keep
following the default, so a later change of the default still reaches
them. This differs from the rules, where a Member's own rules replace
the default rules as a whole. A disabled user may do none of it.

An Owner or the operator can also set single switches for an
Administrator (an Administrator manages Members alone, so never another
Administrator's switches). The default is for patrons: it never limits
an Administrator, whom only the switches set for them alone limit.
Owners are never limited.

"Link store accounts" replaces the earlier per-member rule "may connect
store accounts here". Like that rule it is checked again inside each
account's import batch, so turning it off stops a join that is still
adding accounts.

### One authority per linked person

A person linked between Bookclerk and another server has one
**authority**: the side their account came from. The authority's scope
and suspension are copied to the other side, which enforces them.
Bookclerk never writes back to its own authority. When a person is
linked in both directions, the side the account was created on stays the
authority, and an Administrator can switch it.

- **The other server is the authority.** Its plugin translates the
  person's permissions into a scope at each sign-in and each user poll.
  The host stores it on the patron as managed, so it can't be edited in
  Bookclerk. When the person is disabled, locked or loses every mapped
  library there, the host disables the patron, which ends their sessions
  and library cards. A guest account there is never admitted.
- **Bookclerk is the authority.** When Bookclerk's provider issues a token
  to a plugin's client, the plugin adds the claims its server reads (for
  Audiobookshelf, the group and advanced permissions claims). Owners and
  Administrators map to `admin`, Members to `user`. The plugin also pushes
  scope changes and suspensions to the server's API at once, so they
  don't wait for the next login. The token endpoint already refuses a
  disabled patron.

Audiobookshelf's permissions hold less than a scope: a list of
libraries, one list of tags that either allows or denies, and the
explicit switch. From Audiobookshelf to Bookclerk nothing is lost:
libraries become collection includes and the tag list becomes tag
includes or excludes. From Bookclerk to Audiobookshelf, collection rules
become the library list, and the plugin expresses the rest with tags of
its own: for each distinct scope it computes the books that pass and
keeps a `bookclerk:scope-<id>` tag on exactly those Audiobookshelf items,
then gives the person an allow list of that one tag. It updates the tags
when a scope changes and when items are added or their metadata
changes. Tags Audiobookshelf users set themselves are left alone.

The claim formats belong to the other server, so plugins build them; the
host only stores scopes and asks for claims. The plugin interface grows
by appending to `plugin.capnp`: a user report carries a scope and a
status (new, changed or withdrawn), and an OpenID client template can
declare extra claims that the plugin fills at token time.

### Library mappings

The Audiobookshelf plugin replaces its single `library_id` setting with a
table of mappings: an Audiobookshelf library and folder, the Bookclerk
storage key prefix it shows, and the Bookclerk collection it corresponds
to. Keying on the storage key, rather than Bookclerk's own path, lets the
two servers mount the same files at different paths. There is no
migration from `library_id`; there has been no release.

## Consequences

- Patron scopes are a host feature with uses beyond Audiobookshelf: Plex,
  Jellyfin or any OpenID client can carry the same scope.
- Every library query takes the scope filter, so a new screen that
  forgets it shows too much. The filter lives in one place, and tests
  check each screen against a scoped Member.
- When Bookclerk is the authority, the Audiobookshelf plugin writes its
  own tags onto Audiobookshelf items, so its API key needs permission to
  update items, and those tags are visible to Audiobookshelf admins.
- A scope can't restrict an Audiobookshelf admin, and linking
  Audiobookshelf's `root` account to a Bookclerk Member would deny that
  login. The plugin warns before such a link.
- A suspension made in Audiobookshelf reaches Bookclerk at the next user
  poll, which the live channel usually makes a matter of seconds.
- Settings gain a `table` type for the mappings. Rows are records on the
  wire and in storage, typed per SDK, so adding a column keeps old rows
  working; the CLI also takes a row in column order.
