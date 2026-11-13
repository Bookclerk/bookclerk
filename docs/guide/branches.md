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
sees whose account it's on. Nothing is copied until someone asks for it.

## See your books without downloading them

A server can show the books you have in each library you joined without
keeping copies. Open **Settings → Library** and turn off **Use this
computer to download** for that library. Your server is then a window of
it:

- It runs none of that library's work, and anything in progress is handed
  back.
- **Library** shows **Your books in other libraries**: the books each
  library holds for you, merged into one list. The same book in two
  libraries shows once, with both libraries named.
- The search box on **Library** narrows this list too.
- The list is refreshed when your server connects, every 30 minutes, and
  when you change the setting. If a refresh fails, the last list stays
  with a **Try again** button.

Turn the setting back on to download again. The list for that library is
then cleared, even while the library can't be reached.

> [!NOTE]
> Asking a library to send you one of these books isn't possible yet.

## Leave a library

On the branch, open **Settings → Library → Leave library**.

The library's operator or an Owner can also remove a branch from
**Settings → Nodes**.

## Clerks

A **clerk** is different from a branch. It is an extra copy of the library
itself, sharing the library's database, so the library runs on more than
one machine. Setting up clerks is an operator task; see [Libraries and branches](../server/cluster-hosts.md#clerks).
