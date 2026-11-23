# Sharing with your household

One Bookclerk library can serve a whole household. Everyone sees the same
library, keeps their own wishlist and store accounts, and signs in as
themselves.

## Invite someone

An Owner or Administrator does this:

1. Open **Settings → User Management**.
2. Add the person with their email address, and pick a role (see
   [Roles](web-app.md#roles)).
3. Copy the invite link Bookclerk shows, and send it to them.

When they open the link, they choose a password or a passkey, and they are
in.

An invite link works once and expires. If it has been used or has run out,
open the person's actions in **User Management** and send a new one.

> [!NOTE]
> For invite links to work from other devices, the library needs an address
> people can reach, such as `https://books.example.com`. The operator sets
> it; see [Integrations and sign-in](../server/integrations.md).

## What people share, and what they keep

| Shared with everyone | Each person's own |
| --- | --- |
| The library: every book anyone's accounts own | Their store accounts and passwords |
| The Global queue of wanted books | Their wishlist |
| Where books are saved | Their Discover rows and preferences |

Each person connects their own store accounts on **Accounts**. Their books
join the shared library; their passwords stay theirs.

## Limit what a patron sees

An Owner or Administrator can limit which books a Member sees, for
example to keep a child to the children's shelf. This is their **patron
scope**. Owners and Administrators always see the whole library.

As with other changes to a person, an Administrator can set the scope of
a Member. Anyone who can set scopes can edit the library's default.

A scope is a list of rules. Each rule names a field (genre, series,
author, narrator, library tag or collection), one value of it, and whether it **shows
only** books with that value or **hides** them:

- With no rules, every book is visible.
- With any "show only" rule, a book needs to match at least one of them.
- A "hide" rule always wins.
- Values match without case, so `kids` matches `Kids`.
- A scope holds up to 16 rules, fewer when their values are long.

A scope can also **hide books marked explicit**: books a store (such as
Audible) or a metadata file marks as explicit content. Books with no mark
stay visible, since not every store says. Out of the box explicit books
are shown.

Books a person holds through their own store accounts are always visible
to them, whatever their scope says.

To set scopes:

1. Open **Settings → User Management**.
2. With no one selected, edit **Default patron scope**. Everyone without a
   scope of their own sees this; out of the box it has no rules.
3. To give one person their own, select them, choose **Own scope** under
   **What this patron sees**, add rules (and tick **Hide books marked
   explicit** if you want), and save. **Follow the library's
   default** returns them to the default.

If another Administrator saved a change to the same scope after you
opened it, Bookclerk saves nothing and says so; open the scope again to
see their change before you make yours.

A scope applies to the Library page: the list, search, filter menus, book
pages and covers. It also applies to **Discover**: its shelves and its
store search show a patron only titles inside their scope. Those titles
are judged by what the store says about them (authors, narrators,
series, genres and the explicit mark), because the library doesn't hold
them. A title can't carry a library tag or a collection, so a scope that
only includes certain tags or collections shows no store titles at all.

The scope also decides what a patron can take with them. Playing a book,
downloading a copy to a branch or a clerk, and a player app signed in
with a library card all see only the books inside the scope, and an app
only learns of listening progress and bookmarks for those books. The
library checks this itself, so a clerk or branch never hears of the
rest. Books that only a branch holds are judged by what the branch says
about them, as Discover judges store titles. A person's own wishlist
doesn't apply the scope yet.

From the command line, `bookclerk users scope get` and
`bookclerk users scope set` do the same (see the
[CLI reference](../server/cli-reference.md)).

### Library tags

Genres come from the stores and don't always fit your household. A
**library tag** is a label you choose, such as `Kids` or `Bedtime`, which
everyone sees and a scope can show or hide by.

1. Open a book on the **Library** page.
2. Under **Library tags**, type the tags, separated by commas, and choose
   **Save tags**.

Only an Owner, an Administrator or the operator can set them, and an
Owner can turn that off for one Administrator with the **Set patron
limits** switch (see
[Limit what an Administrator may do](#limit-what-an-administrator-may-do)).
Anyone can filter the Library by them under **Library tags**. They are
separate from **My tags**, which each person keeps for themselves.

A book's tags can also come from a sidecar metadata file beside it. Those
are listed under the tags you set and can't be edited here.

From the command line, `bookclerk library tags get` and
`bookclerk library tags set` do the same.

### Collections

A **collection** is a named part of the library made of storage folders,
for example `Kids` for everything under `kids/`. A book with a file under
one of a collection's folders is in it, so the Library can filter by
collection and a scope can show or hide one.

1. Open **Settings → User Management**.
2. With no one selected, under **Collections**, add a collection: a name
   and its folders, separated by commas, relative to the storage root.
3. Choose **Save collections**.

A library holds up to 32 collections of up to 8 folders each. A folder
matches whole path segments, so `kids` covers `kids/Gruffalo.m4b` but not
`kidsbooks/`. A book held only through a store account has no file until
it is downloaded, so it is in no collection until then.

Saving a change re-indexes the Library in the background, and the filter
and scopes follow once that finishes. Who may change collections is the
same as for library tags. From the command line,
`bookclerk library collections get` and
`bookclerk library collections set --collection 'Kids=kids/,children/'`
do the same.

## Limit what a patron may do

Six switches, the **member controls**, say what a Member may do. All are
on out of the box except **Upload**, which is off until you turn it on.

| Control | Turning it off stops |
| --- | --- |
| Link store accounts | Connecting a store, signing in to one again, and bringing connections from another library. Connections already here keep working. |
| Download | Taking copies of the library's files to a branch or a clerk app. Listening in the web app stays allowed |
| Edit metadata | Changing a book's details |
| Upload | Adding books by uploading files, folders or ZIP and tar archives |
| Request books | Asking a branch for a book it listed |
| See prices and store links | Prices and store links in Discover and the wishlist |

> [!NOTE]
> Members can't edit metadata in Bookclerk yet, so that switch changes
> nothing today. It is kept so the setting is ready when it applies.

With **See prices and store links** off, Discover, catalog search and
the wishlist still show which stores carry a book, but no price and no
link into the store.

To set them:

1. Open **Settings → User Management**.
2. With no one selected, switch **Default member controls**. Every Member
   follows these unless a control is set for them alone.
3. To change one person, select them and pick **Allowed** or **Off** for a
   control under **What this patron may do**. **Default** makes that
   control follow the library's default again.

Each control saves on its own, so two Administrators changing different
controls at the same time keep both changes.

An Owner can also turn single controls off for an Administrator, under
**What this Administrator may do**: for example, to stop them linking
store accounts. Administrators don't follow the members' default, and
only an Owner or the operator can change their controls. Owners may
always do everything. From the command line,
`bookclerk users controls get` and `bookclerk users controls set` do the
same.

## Limit what an Administrator may do

Seven switches, the **Administrator switches**, say what an
Administrator may do. Only an Owner or the operator can change them, and
all are on out of the box. Owners are never limited.

| Switch | Turning it off stops |
| --- | --- |
| Manage members | Inviting, adding, changing, resetting the password of, or removing Members. An Administrator can still change their own profile. |
| Set patron limits | Changing a Member's scope or member controls, the library's defaults for them, and library tags |
| Scan and acquire library-wide | Scanning or acquiring with every store account. They still reach the accounts linked to them. |
| Run the job queue | Cancelling jobs, retrying, cancelling or resuming event deliveries, and starting a listening sync |
| Fix the catalog | Pinning a listing to an edition, removing a pin, splitting a work, and editing or pinning an edition's chapters. The conflicts list and chapter claims stay readable. |
| Invite branches | Creating invites that let a new branch join the library |
| Approve interlibrary requests | Answering requests from other libraries |

> [!NOTE]
> Nothing asks an Administrator to approve an interlibrary request yet, so
> that switch changes nothing today. It is kept so the setting is ready
> when it applies.

To set them:

1. Sign in as an Owner and open **Settings → User Management**.
2. With no one selected, switch **Default Administrator switches**. Every
   Administrator follows these unless a switch is set for them alone.
3. To change one Administrator, select them and pick **Allowed** or
   **Off** for a switch under **Administrator switches**. **Default**
   makes that switch follow the library's default again.

An Administrator sees their own switches but can't change them. From the
command line, `bookclerk users admin-controls get` and
`bookclerk users admin-controls set` do the same (see the
[CLI reference](../server/cli-reference.md)).

## Audiobookshelf

If your household listens with [Audiobookshelf](https://www.audiobookshelf.org/),
Bookclerk can work with it:

- Save books straight into the folder Audiobookshelf reads, and ask it to
  rescan when a new book arrives.
- Let people sign in to Bookclerk with their Audiobookshelf account.
- Use what people listen to in Audiobookshelf to improve Discover.
- Let people sign in to Audiobookshelf with their Bookclerk account.

The operator sets this up; see [Integrations and sign-in](../server/integrations.md).

## A second computer

If someone in the household has their own computer that should download
their books, it can join your library as a **branch**. See
[Join a library as a branch](branches.md).
