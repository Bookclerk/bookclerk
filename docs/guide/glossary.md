# Glossary

The words Bookclerk uses, in plain language.

## Your library

- **Library**: One Bookclerk, with its books and its people. Everyone in a
  household usually shares one.
- **Store**: Where you buy audiobooks: Audible, Libro.fm, Chirp or
  GraphicAudio. The docs for operators also call a store a **source**.
- **Account**: Your sign-in at a store, connected on the **Accounts** page.
- **Scan**: Reading the list of books your store accounts own. Nothing is
  downloaded.
- **Acquire**: Downloading a book and saving it as an audiobook file.
- **Destination**: A place where Bookclerk saves books: a folder or cloud
  storage.
- **Naming profile**: The rules for folder and file names, such as
  **audiobookshelf** or **classic**.
- **Wishlist**: Books you want but do not own yet.
- **Global queue**: Every book anyone in the library has on their wishlist,
  most wanted first.

## People and sign-in

- **Owner, Administrator, Host, Member**: People's roles in a library. See
  [Roles](web-app.md#roles).
- **Operator**: Whoever runs the server, using the operator token. Not a
  person's account.
- **Operator token**: The key to the server itself, printed by
  `bookclerk daemon token`.
- **Invite link**: A one-time link that lets a new person set up their
  account.
- **Passkey**: A way to sign in with your phone, computer or security key
  instead of a password.

## Behind the scenes

- **Plugin**: An add-on that teaches Bookclerk something, such as a store or
  a destination. Plugins run locked down and only reach what they were
  allowed to.
- **Branch**: Another Bookclerk install that joins a library to do work for
  it. See [Join a library as a branch](branches.md).
- **Clerk**: An extra copy of the library itself on another machine, sharing
  its database.
- **Node**: Any computer running Bookclerk for a library: the library's own,
  its clerks and its branches.
- **Daemon**: `bookclerkd`, the Bookclerk program that keeps running and
  serves the web app.
- **Files folder**: The folder where Bookclerk keeps its settings, database
  and downloads (`BookclerkFiles` by default).
