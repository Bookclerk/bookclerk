# Getting started

Bookclerk runs on your own computer or server and gathers the audiobooks you
bought from Audible, Libro.fm, Chirp and GraphicAudio into one library. You
use it in a web browser. This page takes you from a fresh install to your
first books.

Bookclerk has no stable release yet. Until it does, you build the
development version yourself, and things may change between updates.

## 1. Install and start Bookclerk

You need [Rust](https://rustup.rs/), [Node.js](https://nodejs.org/) and git.
Then, in a terminal:

```sh
git clone https://github.com/Bookclerk/bookclerk
cd bookclerk
cargo dev --optional
```

`cargo dev` builds Bookclerk and starts it. `--optional` adds the store
plugins (Audible, Libro.fm, Chirp, GraphicAudio). The first build takes a
while. When it is done, Bookclerk keeps running in that terminal and your
library is at <http://localhost:8787>.

Your library, settings and downloads live in a folder called
`BookclerkFiles` inside the checkout. To run Bookclerk as a service, with
Docker, or on another machine, see [Running Bookclerk](../server/operations.md).

## 2. First start

The first time you open Bookclerk, it asks for the **operator token**: the
key that proves you are the person who runs this install. Print it in a
second terminal:

```sh
cargo dev-cli -- daemon token
```

Paste it in, and Bookclerk asks how this server should start:

- **Start a new library** is what you want the first time.
- **Join a library** connects this install to a library someone already
  runs. See [Join a library as a branch](branches.md).

## 3. Create your own account

The operator runs the server, but cannot connect stores or own books. In
**Settings → User Management**, create the first **Owner**: that is you.
Bookclerk gives you an invite link. Open it, choose a password or a
passkey, and sign in as yourself from then on.

On a computer with a desktop, the Bookclerk icon in the system tray can
also open the library or copy a sign-in link for the operator.

## 4. Turn on your stores

Each store is a plugin, and plugins run only once the operator allows them.
Once an Owner exists, the operator token no longer signs in to the web app.
Instead, as the Owner, open **Settings → Account** and use **Operator
elevation** (your password or passkey) to unlock the operator settings for
this session. Then open **Settings → Plugins** and enable each store you buy
from. Bookclerk first shows what that plugin asks for,
such as the store's web addresses it needs to reach; approve it to turn the
store on.

## 5. Connect a store

Sign in as yourself, open **Accounts** and choose **Connect** next to a
store:

- **Audible** sends you to Amazon to sign in. If your Amazon account uses
  two-step verification, enter the code there as usual.
- **Libro.fm, Chirp and GraphicAudio** ask for your email and password for
  that store. Bookclerk stores the password encrypted and never shows it
  again.

More about each store: [Stores](stores.md).

## 6. Bring in your library

On the **Library** page, choose **Scan**. Bookclerk reads the list of books
each connected account owns. Nothing is downloaded yet.

When you are ready, choose **Acquire pending** to download your books.
Bookclerk saves each one as an audiobook file in the `Audiobooks` folder
inside `BookclerkFiles`. To save them somewhere else, see
[Where your books go](where-books-go.md).

Bookclerk does not download anything on its own until you turn on
**Auto acquire** in **Settings → Server Settings**. Leave it off until you
are happy with where files land.

## Next steps

- [Discover and wishlist](discover.md): find what to listen to next.
- [Sharing with your household](sharing.md): invite family members.
- [The web app](web-app.md): a tour of the screens.
- [Coming from Libation](from-libation.md): bring over an existing library.
- Prefer the command line? See
  [Build from source and use the CLI](../contributing/build-from-source.md).
