# Join a library as a branch

A **branch** is a second Bookclerk install, on another computer, that joins
a library to help with its work. The **library** is the Bookclerk everyone
in a household signs in to.

A branch downloads books for its owner's store accounts on its own machine,
and sends the results back to the library.

You might add a branch when:

- your books should download on your own computer rather than the shared
  server;
- the library runs on a small server, and a stronger computer should do
  the heavy work.

> [!NOTE]
> A branch keeps its own database and its own operator. It never holds the
> library's database password or encryption keys.

## Join from a new install

1. Install Bookclerk on the second computer and paste its operator token
   (steps 1 and 2 of [Getting started](getting-started.md)).
2. On the welcome screen, choose **Join a library**. On an install that is
   already set up, use **Settings → Library → Join a library** instead.
3. Enter the library's address, for example `https://books.example.com`.
4. If the library uses its own certificate, Bookclerk shows its
   fingerprint. Check that it matches the one on the library's
   **Settings → Branches** page, and confirm.
5. Name the branch and choose **Sign in**. Your browser goes to the
   library.
6. Sign in there as yourself. To have your store connections on the
   branch too, tick **Bring my store connections to this branch** (see
   [Your store connections](#your-store-connections)). Then choose
   **Allow**.
7. You are back on the branch, which now says it is connected, and how
   many store connections it added.

### Join with an invite

If you cannot sign in to the library yourself, someone who can sends you an
invite instead:

1. They open **Add a branch**, in their menu or in **Settings → User
   Management**, and send you the invite.
2. In step 3 above, you use the **I have an invite** tab.

An invite works once and expires, after a day by default.

## Your store connections

Your store accounts can follow you to the branch, so you don't sign in to
each store again there. On the library's **Allow** page, tick **Bring my
store connections to this branch**. It is off unless you tick it.

- Only stores that let one sign-in be used in several places travel, such
  as Libro.fm, Chirp and GraphicAudio. The page lists which of yours will.
- A store that keeps one sign-in, such as Audible, stays on the library.
  Connect it on the branch yourself if you want it there.
- On the branch, the connections are added for its owner. An account
  someone else already connected there is left as it is.
- The branch's operator decides whether you may connect store accounts
  there at all. If they turned that off for you, nothing is added.

To add a connection to the library later, open **Accounts** on the branch
and choose **Add to *library*** beside it. The library's operator rule
applies there too. Adding later, and **Revoke everywhere** below, work only for
the Owner who joined the library from the branch.

On the branch, **Revoke here** disconnects an account only on the branch.
**Revoke everywhere** also disconnects it on every library you added it
to, or brought it from. If a library can't be reached, the page says so;
disconnect it there yourself. Disconnecting on the library doesn't reach
the branch yet.

## Approve the library's plugins

The library tells the branch which plugins it needs, such as the stores you
use. Nothing runs on the branch until its operator approves each one in
**Settings → Library**.

The same page shows what each plugin may reach, and lets you limit it.

## What the library can ask of a branch

A branch you joined as a Member receives only work for your own store
accounts. A branch joined by an Administrator or Owner can receive any
work from the library.

## What the library can see of your books

A library can ask your branch whether it can supply a book its people are
looking at. Its people then see **Available through** your branch on that
book in [Discover and search](discover.md#available-through-a-branch).

Your branch answers with the books in its own library: always yours, and
your members' books unless you turn off **List my members' books** for that
library in **Settings → Library**. Each member can also keep their own books
out with **List my books to joined libraries** in **Settings → Account**.
With **Share this branch's books** off, your branch lists nothing to that
library.

A change takes effect at your branch's next answer, so the library can
show your branch as a source for up to two days afterwards.

The library sees the book and that your branch can supply it. It never
sees whose account it's on. Nothing is copied until someone asks for it:
someone on the library requests the book, and your branch sends it
automatically while it still lists it.

The library's operator can also choose to receive all of your branch's
listed books, not only yours. Your branch then also sends the books your
members downloaded on it, under the same settings: nothing while **Share
this branch's books** is off, no member's book while **List my members'
books** is off, and nothing of a member who opted out. Your branch doesn't
download anything extra for this.

## See your books without downloading them

A server can show the books of each library you joined without keeping
copies. Open **Settings → Library** and turn off **Use this
computer to download** for that library. Your server is then a window of
it:

- It runs none of that library's work, and anything in progress is handed
  back.
- **Library** shows **Books in other libraries**: every book in each
  library, as you would see it on that library's own Library page, merged
  into one list. Books on your own store accounts there are marked
  **Yours**, and **Only yours** shows just those. The same book in two
  libraries shows once, with both libraries named.
- The search box on **Library** narrows this list too.
- The list is refreshed when your server connects, every 30 minutes, and
  when you change the setting. If a refresh fails, the last list stays
  with a **Try again** button.

Turn the setting back on to download again. The list for that library is
then cleared, even while the library can't be reached.

The desktop app starts this way: a library you join from it has the
setting off from the start, so the computer is a window of that library
until you turn downloading on.

> [!NOTE]
> Asking a library to send you one of these books isn't possible yet.

## Sign in with a library card

To see a library's books without joining it as a branch, sign in to it
with a library card. Your server is then a clerk of that library: it shows
the library's books, yours marked, and never takes its work.

1. Open **Settings → Library → Library cards** and choose **Sign in with a
   library card**.
2. Enter the library's address, the one you open it with in a browser,
   and choose **Continue**.
3. If the library uses its own certificate, check that the fingerprint
   matches the one on the library's **Settings → Branches** page, and tick
   **It matches**.
4. Name this computer. The library shows the name when it asks you to
   approve the card.
5. Choose **Sign in to** *library*, sign in on the library with your
   account there, and approve the card.

The library sends you back to your server, which says **Signed in to**
*library*. Within a minute, **Library** shows that library's books under
**Books in other libraries**, yours marked, beside the books of any library
your server is a window of.

To stop, choose **Sign out** beside the library in the same list. Your
server forgets the card and the books, and asks the library to end the
card too. If the library can't be reached, end it later from your session
list on the library. If the library ends the card first, the list says so
and offers **Remove**; sign in again to get a new card.

A server that is already a branch of a library shows your books there
through that link, so it can't also hold a card for it.

## Core and local branches

Whether other computers can reach an install is a separate question from
whether it is a library or a branch:

- A **core branch** has a public address (its `public_origin`, see
  [Configuration](../server/configuration.md)). Other installs can join
  it, and outside services such as cloud storage can send it
  notifications.
- A **local branch** has no public address, such as a computer at home.
  It connects out to its library and never waits for anyone to connect to
  it.

A library that others join must be a core branch. A branch is usually a
local branch, but it can be a core branch too.

## Leave a library

On the branch, open **Settings → Library → Leave library**.

The library's operator or an Owner can also remove a branch from
**Settings → Nodes**.

## Shared branches

A branch can also share the library's own database instead of keeping one
of its own. It is then an extra copy of the library itself, so the library
runs on more than one machine. Setting one up is an operator task; see
[Shared branches](../server/cluster-hosts.md#shared-branches).
