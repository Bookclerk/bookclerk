# ADR: Library catalog network

- **Status:** Accepted (2026-11-11). Step 1, per-library plugin state on a
  branch, and step 7, capacity shares, are implemented. Of step 2, the
  branch-side work scope and copy settings are implemented; the import of
  the owner's own books and sharing only the owner's books are next. The
  other steps are planned and listed under [Consequences](#consequences).
- **Date:** 2026-11-11
- **Related:** [Control plane](control-plane.md) (H-4 branches, branches of
  branches), [Event delivery model](event-delivery-model.md),
  [Library data model](library-data-model.md),
  [Libraries and branches](../server/cluster-hosts.md)

## Context

One install can join several libraries as a branch. Each active
`cluster_links` row already gets its own client, consent list and catalog
sync, and the join cycle check takes the union of every link's ancestry.
What the earlier records did not settle is how those links relate to each
other and to the branch's own library:

- Which plugins and plugin state belong to which library.
- Whose events a branch's plugins see.
- Which books a branch downloads, which it sends to each library, and
  whether one library can reach books another library holds.
- Where a person's store accounts live when they belong to several
  libraries.

Main left four leaks between links. Guests of the same plugin key shared
one `plugin-state/<PluginKey fs-id>` directory whichever library they ran
for. A branch kept a copy of every book a library asked it to acquire,
other members' books included. Books a branch's owner acquired for a
library never reached the branch's own library. And every library treated
the branch's whole machine as its own.

People also use joins in different ways. Some want a local install that
only shows the libraries they belong to in one view. Others want an
archive of their own books that can also fetch books from the libraries
they joined.

## Decision

Libraries form a catalog network, like interlibrary loan between real
libraries. Each library keeps its own collection, people and accounts. A
library asks the libraries linked to it what they hold, and a member asks
explicitly for a copy.

### Libraries are boundaries

- **One library, one boundary.** Events, accounts, books and plugin
  state belong to one library. Nothing crosses a link except through a
  typed, consented node-socket frame, and every crossing becomes an
  ordinary local change, with its own events, on the receiving library.
- **Trust is one hop.** A library knows only its direct branches. A
  branch answers for itself and never names where it got something.
- **A branch is a library plus one worker per link.** Its own library has
  its own users, accounts, plugins and events. Each library it joined
  gets a separate worker that runs only that library's jobs, under that
  library's consent, with that library's handed credentials.

### Plugins and plugin state

- A library's plugins on a branch belong to that link. They run under the
  branch operator's consent for that link.
- Their host-managed state lives in a subtree per library,
  `plugin-state/link-<32 hex>/<PluginKey fs-id>/`, where the leaf is the
  first 128 bits of the SHA-256 of the library's cluster id. The branch's
  own guests keep `plugin-state/<PluginKey fs-id>/`. So no guest shares a
  `HOME` or `TMPDIR` with another library's copy of the same plugin.
- The read-only plugin code cache stays shared, because it is
  content-addressed by digest.
- When a link is left or revoked, its subtree is deleted once its client
  has stopped. Each plugin's Windows ACL journal is revoked first, as
  `--purge-state` does.

### Events and jobs

- Events never cross a library boundary, as the
  [Event delivery model](event-delivery-model.md#decision) states. A
  library's plugins running on a branch are job runners, not event
  subscribers. If event delivery to branches is ever built, a delivery
  is leased to that library's deployment on the branch node and runs in
  that link's sandbox, never in the branch's own plugins.
- A job belongs to the library that queued it, runs only in that link's
  worker, and is never re-routed to another link.

### Books

- **A branch downloads automatically only for its own library**: its
  owner and its own members. A library's jobs for other members of that
  library run only when the branch's operator turns that on for the link.
- **A book belongs to the store account that bought it.** A book the
  branch's owner acquires for a library is imported into the branch's own
  library as well.
- **A library automatically receives only the branch owner's books.** A
  library's operator may also choose to acquire all of a branch's
  members' books automatically. That covers only books the branch lists
  to it.
- **Everything else a branch can reach is listed, not copied.** A library
  asks its branches by strong identifiers (edition ASIN or ISBN with
  marketplace, work identifiers), never by title. Its own work id is only
  a correlation key, since work ids are not shared between libraries.
  Questions are stored on the library and pulled by the branch. Answers
  are recorded as listings from source `branch:<node>` in the shared
  listing pool, so search and Discover show them with no special case.
- **A member's request is a job routed to the branch**, fulfilled
  automatically for anything the branch listed. The book lands in the
  branch's library first, then reaches the library through the existing
  upload path. It is held there as a node holding, credited to the branch.
- **Each branch operator decides where the edge stops.** Per link, the
  branch chooses what it lists: its members' books (on by default, each
  member can opt out) and what it can reach in its other libraries (off).
- **Onward lending is off by default.** A library's books are listed to
  its members' other libraries only after that library's operator turns
  it on.

### Accounts

- A person's store accounts follow them to every instance they connect.
  Joining offers to bring the person's own accounts, never another
  member's. Each operator decides, per member, whether accounts may be
  connected there.
- A storefront plugin declares in `plugin.toml` whether its credentials
  are `shareable` (each instance refreshes on its own) or
  `single_keeper`. A single-keeper account has one instance holding its
  refresh lease, the person's branch when there is one, which pushes new
  tokens to the others over the person's links.

### Capacity

A branch of several libraries splits its machine so that no one library can
take all of it.

- The machine's job slots come from the branch's own heartbeat: its CPU
  budget over one default guest (0.8 of a core) and its memory budget over
  one default guest (512 MiB), whichever is smaller, from 1 to 16. A
  drained branch has none.
- The branch's own work comes first: each job its own workers run takes a
  slot.
- The slots left are dealt out one at a time, in cluster id order, to each
  joined library still under its cap. Shares are equal; a capped library's
  unused slots go to the others. The branch's operator or an Owner sets the
  cap per library (0 to 16, or none).
- The branch claims no more of a library's jobs than its share. A smaller
  share cancels nothing; claims wait until enough jobs finish.
- The branch's heartbeat to each library reports that library's share of
  the machine's CPU and memory, and of that share only what is free now,
  so a library's scheduler never counts on the whole machine. The totals
  stay physical because the library applies its own budget for the node
  on top; the branch's budget bounds the work through the slots.

### Kinds of install

A **window** shows the libraries a person joined in one view and keeps no
downloads. A **branch** archives its own library's books. Both are the
same software: a per-link setting, "Use this computer to download",
decides whether the link takes work.

## Consequences

- Built in order, one pull request each:
  1. Per-library plugin state and this record.
  2. Own library first: the branch-side work scope and copy settings, the
     import of the owner's own books, and sharing only the owner's books.
  3. Accounts that follow the person, with the `credentials` declaration.
  4. Catalog lookup by strong identifiers into the listing pool.
  5. Interlibrary requests, including the library operator's "acquire all
     of this branch's members' books" setting.
  6. Window mode.
  7. Capacity shares between a branch's own work and each link
     (implemented).
- Steps 2 to 7 add node-socket frames. Each ships with a fuzz target, a
  seed corpus and a replay test. `plugin.capnp` does not change.
- Books held through a branch become node holdings in the
  [library data model](library-data-model.md), not new tables.
- A branch that leaves a library loses that library's plugin caches and
  logins on the branch. Rejoining starts them fresh.
