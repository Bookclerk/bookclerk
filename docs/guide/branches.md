# Join a library as a branch

A **library** is the Bookclerk everyone in a household signs in to. A
**branch** is a second Bookclerk install, on another computer, that joins
that library to help with the work. It downloads books for its owner's
store accounts on its own machine and sends the results back to the
library.

You might add a branch when:

- your books should download on your own computer rather than the shared
  server;
- the library runs on a small server, and a stronger computer should do
  the heavy work.

A branch keeps its own database and its own operator, and it never holds
the library's database password or encryption keys.

## Join from a new install

1. Install Bookclerk on the second computer ([Getting started](getting-started.md),
   steps 1 and 2).
2. On the welcome screen, choose **Join a library**. (On an install that is
   already set up: **Settings → Library → Join a library**.)
3. Enter the library's address, for example `https://books.example.com`.
4. If the library uses its own certificate, Bookclerk shows its
   fingerprint. Check it against the one on the library's
   **Settings → Branches** page, and confirm they match.
5. Name the branch and choose **Sign in**. Your browser goes to the library;
   sign in there as yourself and choose **Allow**.
6. You are back on the branch, which now says it is connected.

If you cannot sign in to the library yourself, someone who can opens
**Add a branch** for you (in their menu, or in **Settings → User
Management**) and sends you the invite. Use the **I have an invite** tab in
step 3 instead. An invite works once and expires, after a day by default.

## Approve the library's plugins

The library tells the branch which plugins it needs, such as the stores you
use. Nothing runs on the branch until its operator approves each one in
**Settings → Library**, where you can also see and limit what each plugin
may reach.

## What the library can ask of a branch

A branch you joined as a Member receives only work for your own store
accounts. A branch joined by an Administrator or Owner can receive any
work from the library.

## Leave a library

**Settings → Library → Leave library** on the branch. The library's
operator or an Owner can also remove a branch from **Settings → Nodes**.

## Clerks

A **clerk** is different: an extra copy of the library itself that shares
the library's database, for running it on more than one machine. Setting up
clerks is an operator task; see [Libraries and branches](../server/cluster-hosts.md#clerks).
