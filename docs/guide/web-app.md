# The web app

You use Bookclerk in your web browser. On the computer that runs it, open
<http://localhost:8787>; from other devices, use the address your operator
gives you.

## Screens

- **Discover**: suggestions and catalog search across every store. See
  [Discover and wishlist](discover.md).
- **Library**: every book in the household's library, whoever bought it.
  People allowed to download books also see **Scan** and **Acquire
  pending** here.
- **Wishlist**: your wishlist and the Global queue everyone shares.
- **Accounts**: connect and disconnect your store accounts. See
  [Stores](stores.md).
- **Status**: for Hosts, Administrators, Owners and the operator, the
  health of the server and of every branch.
- **Settings**: your profile, password, passkeys and sessions; and, for
  the people who run the library, users, sign-in options, plugins and
  server settings.

Bookclerk opens on Discover. To start somewhere else, or to switch between
light and dark, open **Preferences** in the top bar.

## Roles

Everyone in a library has a role. It decides what they can do:

| Role | Can |
| --- | --- |
| Member | Browse the library, use Discover and the wishlist, connect their own stores |
| Host | Everything a Member can, plus look after the branches they own |
| Administrator | Everything a Member can, plus scan, download books and add people |
| Owner | Everything an Administrator can, plus manage sign-in options and other Administrators |

The **operator** is not a person in the library but the key to the server
itself: the operator token. It is used to set up the server, turn on
plugins and change server settings. Once an Owner exists, an Owner can
step up to operator powers from their own account when needed.

## Signing in

Depending on how your library is set up, you can sign in with:

- a password, optionally with a code from an authenticator app;
- a **passkey** (your phone, computer or security key), set up in
  **Settings → Account**;
- a button for Google, GitHub, Apple, Discord or another sign-in provider,
  if your Owner turned one on;
- your Audiobookshelf account, if your library is connected to one.

New people join with an invite link; see
[Sharing with your household](sharing.md).

On the computer that runs Bookclerk, the tray icon opens the library and can
copy a one-time sign-in link for the operator.

The full reference for roles, sign-in and settings is
[Web app and sign-in reference](../server/sign-in.md).
