# Stores

Bookclerk works with four audiobook stores today. Each store you connect
adds the books you bought there to your one library.

| Store | How you sign in | What you get |
| --- | --- | --- |
| Audible | Amazon sign-in in your browser | Your Audible library, downloaded as audiobook files |
| Libro.fm | Your Libro.fm email and password | Your Libro.fm library, as M4B files or the store's ZIP |
| Chirp | Your Chirp email and password | Your Chirp purchases |
| GraphicAudio | Your GraphicAudio email and password | Your GraphicAudio purchases |

## Connect a store

Stores are connected by each person for their own accounts, on the
**Accounts** page:

1. Choose **Connect** next to the store.
2. Sign in. For Audible you go to Amazon's own sign-in page; enter your
   two-step verification code there if Amazon asks for one. For the other
   stores, type your email and password for that store.
3. The store appears under **Connections**. Its books show up in the
   library after the next scan.

If a store is missing from Accounts, the operator has not turned it on yet
(**Settings → Plugins**; see [Getting started](getting-started.md#4-turn-on-your-stores)).

Your store passwords are stored encrypted in the library's database. They
are never shown again, and nobody else in your household can see them.

## Disconnect a store

On **Accounts**, remove the connection. Books you already downloaded stay
where they are.

## Store by store

**Audible.** Bookclerk signs in as a phone app would, the way Amazon's own
apps do. If downloads stop working after a while, connect Audible again from
Accounts to refresh the sign-in.

**Libro.fm.** Books download as M4B audiobook files by default. The operator
can switch to the store's ZIP download instead.

**Chirp.** Bookclerk signs in with your Chirp email and password; it does
not need your browser.

**GraphicAudio.** GraphicAudio sells several kinds of access. By default
Bookclerk uses GraphicAudio's web player access, so it
does not use up one of your download attempts or app devices. The operator
can choose the ZIP download or the app's streams instead.

## More stores

Other stores may come later, as plugins. Anyone can write a store plugin;
see [Plugins](../plugins/README.md).

Operators: every store setting, and the command-line equivalents, are in
the [Stores reference](../server/stores.md).
