# Getting started

This page takes you from a fresh install to your first books. Bookclerk runs
on your own computer or server, and you use it in a web browser.

> [!NOTE]
> Bookclerk has no stable release yet. Until it does, you build the
> development version yourself, and things may change between updates.

## 1. Install and start Bookclerk

You need [Rust](https://rustup.rs/), [Node.js](https://nodejs.org/) and git.
Then run these commands in a terminal:

```sh
git clone https://github.com/Bookclerk/bookclerk
cd bookclerk
cargo dev --optional
```

`cargo dev` builds Bookclerk and starts it. `--optional` adds the store
plugins for Audible, Libro.fm, Chirp and GraphicAudio.

The first build takes a while. When it finishes, Bookclerk keeps running in
that terminal, and your library is at <http://localhost:8787>.

Your library, settings and downloads live in the `BookclerkFiles` folder
inside the checkout. To run Bookclerk as a service, with Docker or on
another machine, see [Running Bookclerk](../server/operations.md).

## 2. Unlock the server

The first time you open Bookclerk, it asks for the **operator token**. This
is the key that proves you are the person who runs this install.

1. In a second terminal, print the token:

   ```sh
   cargo dev-cli -- daemon token
   ```

2. Paste it into the page.
3. Choose **Start a new library**.

The other choice, **Join a library**, connects this install to a library
someone already runs. See [Join a library as a branch](branches.md).

## 3. Create your own account

The operator runs the server, but cannot connect stores or own books. So
next you create an account for yourself, as the first **Owner**.

1. Open **Settings → User Management** and create the Owner.
2. Open the invite link Bookclerk gives you.
3. Choose a password or a passkey.

From now on, you sign in as yourself.

> [!TIP]
> On a computer with a desktop, the Bookclerk icon in the system tray can
> open the library, or copy a sign-in link for the operator.

## 4. Turn on your stores

Each store is a plugin, and a plugin runs only once the operator allows it.
Once an Owner exists, the operator token no longer signs in to the web app,
so you unlock the operator settings from your own account:

1. As the Owner, open **Settings → Account** and choose **Operator
   elevation**. Confirm with your password or passkey.
2. Open **Settings → Plugins**.
3. Enable each store you buy from. Bookclerk first shows what the plugin
   asks for, such as the store web addresses it needs to reach.
4. Approve it to turn the store on.

## 5. Connect a store

Sign in as yourself, open **Accounts**, and choose **Connect** next to a
store.

- **Audible** sends you to Amazon to sign in. If your Amazon account uses
  two-step verification, enter the code there as usual.
- **Libro.fm, Chirp and GraphicAudio** ask for your email and password for
  that store. Bookclerk stores the password encrypted and never shows it
  again.

More about each store is on [Stores](stores.md).

## 6. Bring in your library

1. On the **Library** page, choose **Scan**. Bookclerk reads the list of
   books each connected account owns. Nothing is downloaded yet.
2. When you are ready, choose **Acquire pending** to download your books.

Bookclerk saves each book as an audiobook file in the `Audiobooks` folder
inside `BookclerkFiles`. To save them somewhere else, see
[Where your books go](where-books-go.md).

> [!IMPORTANT]
> Bookclerk downloads nothing on its own until you turn on **Auto acquire**
> in **Settings → Server Settings**. Leave it off until you are happy with
> where files land.

## Next steps

- [Discover and wishlist](discover.md): find what to listen to next.
- [Sharing with your household](sharing.md): invite family members.
- [The web app](web-app.md): a tour of the screens.
- [Coming from Libation](from-libation.md): bring over an existing library.
- Prefer the command line? See
  [Build from source and use the CLI](../contributing/build-from-source.md).
