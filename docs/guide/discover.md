# Discover and wishlist

**Discover** suggests what to listen to next, and the **wishlist** keeps
track of books you want but do not own yet, across every store at once.

## Discover

Discover shows rows of suggestions, each with its reason, for example:

- **On the wishlist**: what people in your household want, first.
- **Finish these series**: the next book in a series you started.
- **Pick up where you left off**: books you have started.
- **Because you wishlisted** a title: books related to something on your
  wishlist.
- **More from** an author, **If you like** an author, or **Narrated by** a
  narrator you like.
- **Because you like** a genre you finish books in.
- **Similar to your books and wishes** and **Top picks for you**.
- **Popular in** a genre your books and wishes lean to (or a few common
  genres while Discover is still learning your taste), ranked for you.
- **Picked for you on** a store: the rest of your matches, each under the
  store where it costs you least, and **Chirp deals**.

Each book appears on one row only. Most rows show no more than two books
of the same series, so one long series cannot crowd out the rest. **Finish
these series**, **Pick up where you left off** and **Wishlist** may show
more, taking turns between series, and so may **Top picks for you**.

Each book appears on one row only. Most rows show no more than two books
of the same series, so one long series cannot crowd out the rest. **Finish
these series**, **Pick up where you left off** and **Wishlist** may show
more, taking turns between series, and so may **Top picks for you**.

Suggestions come from the stores' catalogs, so they include books nobody in
your library owns yet. The more you rate and finish, the better they get.
Rows only suggest books in the languages of the books that shape your taste,
or in English when none of those books says its language. A book whose store
does not give its language can still appear.
Until you have a few books or listens of your own, your household's library
shapes your rows too.

If your household uses Audiobookshelf, Bookclerk can also learn from what
you listen to there.

Your rows are built in the background, from your own books, listening and
wishlist, and refreshed every day. The first time you open Discover they may
take a minute; the page shows placeholders and fills in on its own. When you
add a wish, your rows rebuild within a minute, and the page says **Updating
shelves** until they land. **Refresh** asks for a rebuild right away.

To hide rows you do not want, open **Preferences** in the top bar.

## How a book fits your taste

Open a book on Discover, the wishlist or the library and its details show
**How it fits your taste**: a chart with one spoke for each part of your
taste. Bookclerk groups the books you finish, rate and own with the books
you wish for by what they are about, and names each group by what its
books share: a genre, a series, or a phrase from their descriptions. The
shape reaches further along a spoke the closer the book lands to that
group.

Point at a spoke, or tap it, to see how close the book is and which of
your books and wishes it is nearest to. With a keyboard, move between
spokes with <kbd>Tab</kbd>.

The chart appears when your library has an
[embedding model](../server/configuration.md#model-downloads) and your
books and wishes make at least three groups. With one or two groups, each
is shown as a row with a bar instead. Every book in the library counts a
little, so a library nobody has rated yet still has a taste. A wish counts for more
than a book that is only in the library, and for less than a book you
finished or rated well.

Your rows use the same groups: a book close to one of them ranks higher,
and a book close to several ranks higher still.

## Search every store

The search box at the top searches every store's catalog at once. Each
recording shows up once, with where to buy it and the best current price,
even when several stores, or one store under two publishers, sell it.

Search also shows books your library already has. Those say **In your
library**, or **Another edition in your library** when you have the book in
another recording or from another store.

Prices take the stores you have connected into account, so a member price
counts where you are a member.

When your library has an embedding model, results sorted by relevance are
ordered by what you asked for as well as by the stores' own ranking, so a
description such as "a slow mystery in a small town" brings the closest
titles of each page forward. Between results that match equally well, the
one closer to your taste comes first. The suggestions under the search box
keep the stores' order.

## Available through a branch

When your library has [branches](branches.md), a book one of them can supply
says so: **Available through Riley's branch**, on its Discover card, in
search results and in its details. Bookclerk matches the book by its store
listing, never by title, so search shows it for books your library already
knows about, such as ones you own, wish for or see on Discover.

To get the book, open its details and choose **Request from Riley's
branch**. The branch sends it automatically if it still has it, downloading
it first if needed, and it appears in your library after the branch's next
sync. If the branch no longer lists the book, the details say so.

A branch drops off a book within two days of no longer listing it.

## Wishlist

Add a book to your wishlist from any card or search result. Your wishlist is
yours, whichever store the book comes from.

## Global queue

Beside your list, the **Global queue** shows every book that anyone has on
their wishlist, with the faces of the people who want it.

- Books more people want rise to the top.
- When a book on the queue joins the library, it leaves the queue.
- There is no approval step. Whoever buys books for the household can
  simply work down the list.

How suggestions are found and ranked is described in
[Discovery design](../internals/discovery.md).
