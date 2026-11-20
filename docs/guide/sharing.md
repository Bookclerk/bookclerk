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

An Owner or Administrator can limit which books a Member or Host sees, for
example to keep a child to the children's shelf. This is their **patron
scope**. Owners and Administrators always see the whole library.

As with other changes to a person, an Administrator can set the scope of
a Member, while a Host's scope takes an Owner or the operator. Anyone who
can set scopes can edit the library's default.

A scope is a list of rules. Each rule names a field (genre, series, author
or narrator), one value of it, and whether it **shows only** books with
that value or **hides** them:

- With no rules, every book is visible.
- With any "show only" rule, a book needs to match at least one of them.
- A "hide" rule always wins.
- Values match without case, so `kids` matches `Kids`.
- A scope holds up to 16 rules, fewer when their values are long.

Books a person holds through their own store accounts are always visible
to them, whatever their scope says.

To set scopes:

1. Open **Settings → User Management**.
2. With no one selected, edit **Default patron scope**. Everyone without a
   scope of their own sees this; out of the box it has no rules.
3. To give one person their own, select them, choose **Own scope** under
   **What this patron sees**, add rules, and save. **Follow the library's
   default** returns them to the default.

A scope applies to the Library page: the list, search, filter menus, book
pages and covers. Discover, the wishlist, downloads and library cards do
not apply it yet.

From the command line, `bookclerk users scope get` and
`bookclerk users scope set` do the same (see the
[CLI reference](../server/cli-reference.md)).

## Limit what a patron may do

Six switches, the **member controls**, say what a Member may do. All are
on out of the box.

| Control | Turning it off stops |
| --- | --- |
| Link store accounts | Connecting a store, signing in to one again, and bringing connections from another library. Connections already here keep working. |
| Download | Taking copies of the library's files to a branch or a clerk app, and playing them |
| Edit metadata | Changing a book's details |
| Upload | Adding books by uploading files |
| Request books | Asking a branch for a book it listed |
| See prices and store links | Prices and store links in Discover and the wishlist |

> [!NOTE]
> Members can't edit metadata or upload books in Bookclerk yet, so those
> two switches change nothing today. They are kept so the setting is
> ready when it applies.

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

An Owner can also turn single controls off for an Administrator, under
**What this Administrator may do**: for example, to stop them linking
store accounts. Administrators don't follow the members' default, and
only an Owner or the operator can change their controls. Owners may
always do everything. From the command line,
`bookclerk users controls get` and `bookclerk users controls set` do the
same.

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
