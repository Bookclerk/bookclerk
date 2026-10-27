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
6. Sign in there as yourself and choose **Allow**.
7. You are back on the branch, which now says it is connected.

### Join with an invite

If you cannot sign in to the library yourself, someone who can sends you an
invite instead:

1. They open **Add a branch**, in their menu or in **Settings → User
   Management**, and send you the invite.
2. In step 3 above, you use the **I have an invite** tab.

An invite works once and expires, after a day by default.

## Approve the library's plugins

The library tells the branch which plugins it needs, such as the stores you
use. Nothing runs on the branch until its operator approves each one in
**Settings → Library**.

The same page shows what each plugin may reach, and lets you limit it.

## What the library can ask of a branch

A branch you joined as a Member receives only work for your own store
accounts. A branch joined by an Administrator or Owner can receive any
work from the library.

## Leave a library

On the branch, open **Settings → Library → Leave library**.

The library's operator or an Owner can also remove a branch from
**Settings → Nodes**.

## Clerks

A **clerk** is different from a branch. It is an extra copy of the library
itself, sharing the library's database, so the library runs on more than
one machine. Setting up clerks is an operator task; see [Libraries and branches](../server/cluster-hosts.md#clerks).
