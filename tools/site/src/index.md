# Bookclerk: every purchase, properly placed

{{#prerelease}}> **Pre-release.** Bookclerk has no stable release yet. The development version is unstable: settings and the library format may change between updates.

{{/prerelease}}Bookclerk gathers the audiobooks you have bought from Audible, Libro.fm, Chirp and GraphicAudio into one catalog on your own computer, keeps copies you control, and shelves them where your player already looks.

Free and open source. Runs on Linux, macOS and Windows. Works with Audible, Libro.fm, Chirp and GraphicAudio, and more through plugins.

{{#demo}}- [Try the live demo]({{demoUrl}}): your own copy of Bookclerk in the browser for 30 minutes
{{/demo}}{{#prerelease}}- [Try the preview]({{origin}}/download)
{{/prerelease}}{{#released}}- [Download {{releaseTag}}]({{origin}}/download)
{{/released}}- [Documentation]({{origin}}/docs/) ([as Markdown]({{origin}}/docs/README.md), [llms.txt]({{origin}}/llms.txt))
- [Source code on GitHub]({{repo}})

## Three steps from store to shelf

1. **Connect your stores.** Sign in to each store once from the Accounts page. Your sign-ins are encrypted and stay on your machine.
2. **Bookclerk catalogs it all.** Every title you own lands in one searchable library, whichever store you bought it from.
3. **Your copies, where you want them.** Pick a book or let Bookclerk keep up. Files are named, tagged and split into chapters, then saved to a folder or to cloud storage.
{{#demo}}
Try steps 1 and 2 right now: the [live demo]({{demoUrl}}) gives you a private copy of Bookclerk for 30 minutes. Connect a store, scan your library and search it. Downloads are switched off, and your stores are signed out when the demo ends.
{{/demo}}
## A clerk for the books you already paid for

- **One library for every store.** Search, tag and rate across all your purchases instead of hopping between apps.
- **Copies you keep.** Chapters, covers, PDFs and clean metadata. Keep the original format or convert to MP3.
- **Plays where you listen.** Tells Audiobookshelf when new books arrive, so they appear in your player without a manual rescan.
- **Runs on your hardware.** A laptop, a home server or a NAS. Use it from the browser, or script it from the command line.
- **Shared with your household.** Family members get their own sign-in. Owners decide who can add stores and who can only browse.
- **Private by design.** No Bookclerk account to create. Add-ons run sealed off from your files and reach only the sites you approve.

## The library, in your browser

Bookclerk's web interface lists every book in one library with its store and status (acquired, queued, downloading, not acquired). Screenshots of each release will appear here.

## Add a store, a destination or an integration

Plugins are written in TypeScript, Python or Rust against a Workers-style API, and each one runs in its own sandbox with only the access it declares. The [docs]({{origin}}/docs/) cover the architecture, the plugin SDKs and the HTTP API.

```sh
# build and run the development version
git clone {{repo}}
cd bookclerk
cargo dev

# check a plugin before you publish it
cargo plugin -- check ./my-plugin
```

## Before you start

### Can I use Bookclerk today?

{{#prerelease}}You can try the development version, but there is no stable release yet. Settings and the library format may change between updates, and an update can mean starting your library over.{{/prerelease}}{{#released}}Yes. {{releaseTag}} is the current release. Download it from the [download page]({{origin}}/download).{{/released}}

{{#demo}}### Can I try Bookclerk without installing it?

Yes. The [live demo]({{demoUrl}}) gives you your own copy of Bookclerk in the browser for 30 minutes. You can sign in to your stores, scan your library and search it. It cannot download books, and it signs your stores out and deletes everything when the demo ends.

{{/demo}}### Which stores are supported?

Audible, Libro.fm, Chirp and GraphicAudio. You can turn each one on or off, and plugins can add more.

### Do I need a server?

No. Bookclerk runs on an ordinary computer and you use it from your browser. A home server or NAS lets it keep working while your computer is off.

### Is my store password safe?

Your store sign-ins are encrypted and stored in your own library on your own machine. Bookclerk sends them only to the store they belong to, and you can protect the encryption key with a password of your own.

### What can I do with the files?

Keep them as personal copies of books you bought and listen on your own devices. Please do not share them, and check the store's terms and the laws where you live.

### What does Bookclerk cost?

Nothing. Bookclerk is free software under the AGPL. You still buy your books from the stores.
