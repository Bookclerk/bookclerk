# Player apps

Patrons can listen to the library in phone and car apps made for
Audiobookshelf servers. Bookclerk answers those apps itself at `/player`;
no Audiobookshelf server is involved. The decisions are in the
[player API ADR](../adr/player-api.md), and every route is in the
[Web API](api/README.md) reference under **Player**.

## Apps

| App | Platforms | Sign-in |
| --- | --- | --- |
| Audiobookshelf (official) | Android, iOS | Player password or browser sign-in |
| Absorb | Android, iOS | Player password or browser sign-in |
| Lissen | Android | Player password or browser sign-in |
| AudioBooth | iOS | Player password; enter the address by hand |
| Plappa | iOS | Player password |

## Connect an app

1. In the web app, open **Settings**, then **Account**, then **Player
   apps**.
2. Name a player password after the device and select **Make a player
   password**. Copy it: it is shown once.
3. In the app, enter the server address: the library's address with
   `/player` added, such as `https://books.example.com/player`.
4. Sign in with your sign-in name (or email) and the player password.

> [!NOTE]
> Your account password never works in an app. An app that offers a
> browser sign-in (often labelled with the identity provider's button)
> opens the library's own sign-in page instead; approve the app there.

## What apps can do

- Browse the books you can see: your patron scope applies, as on the web.
- Stream any book you can see, and keep your place, finished books and
  bookmarks in step with the web app.
- Save books for offline listening when the **Download** member control is
  on for you.
- Listen offline and sync the progress when back online.

Collections, playlists, podcasts, ebooks and server administration are
not offered to apps.

## Sign an app out

An app shows in **Settings**, **Account**, **Sessions** with a **Player
app** badge, named after its player password or the app. Sign it out
there. Removing a player password signs out every app that used it. An
app also stops working after 30 days unused.

## For operators

- The API is served on the daemon's own listener; nothing extra to
  enable. Put the library behind TLS before exposing it (see the
  [security checklist](security.md)).
- Apps reach the live socket at `/player/socket.io/` over a websocket. A
  reverse proxy must pass websocket upgrades on that path.
- Cards are `portal_sessions` rows of kind `player`; player passwords are
  in `player_passwords` (digests only). See [Database](database.md).
