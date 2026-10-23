# Get Bookclerk

{{#prerelease}}There is no stable release yet. When the first version ships, installers for each system will appear here. Until then you can try the development version if you are comfortable building it yourself.

- **Latest release:** none yet. Watch the [GitHub repository]({{repo}}) to hear when the first release is out.
{{/prerelease}}{{#released}}The current release is {{releaseTag}}. Download it for your system from the release page on GitHub.

- **Latest release:** [{{releaseTag}}]({{releaseUrl}}), released {{releaseDate}}.
{{/released}}- **Development version:** [{{commit}}]({{commitUrl}}), built {{builtOn}}. Unstable.
{{#prerelease}}
## Installers

Available at the first release:

- **Windows:** Windows 10 and 11, 64-bit
- **macOS:** Apple silicon and Intel
- **Linux:** x86-64 and ARM64, with a systemd service
- **Docker:** an image for servers and NAS boxes
{{/prerelease}}
## Build the development version

You need [Rust](https://rustup.rs/), [Node.js](https://nodejs.org/) and git. This builds Bookclerk from the latest code and starts it; the library then opens at `http://localhost:8787`.

```sh
git clone {{repo}}
cd bookclerk
cargo dev
```

[Read the full getting started guide]({{origin}}{{gettingStartedUrl}}) ([as Markdown]({{origin}}/docs/getting-started.md)).
{{#prerelease}}
## What pre-release means

- Settings and the library format can change between updates. An update may ask you to start a fresh library.
- Keep your own backup of anything Bookclerk downloads until there is a stable release.
- Features described on this site may be unfinished or missing in the build you try.
- Please report problems on [GitHub]({{repo}}/issues). Reports from early users shape the first release.
{{/prerelease}}
