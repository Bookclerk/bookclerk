# Plugin registry (crates.io taxonomy + install without Rust)

Third-party plugins remain **prebuilt executables** + `plugin.toml` (see
[plugins.md](plugins.md)). This document defines how publishers advertise them
on [crates.io](https://crates.io) so Bookclerk can **discover** and **install**
them from a packaged binary — without the operator having a Rust toolchain.

> Status: crate naming taxonomy is **draft-stable for discovery**. Install
> metadata, digest requirements, and federated registry identity will evolve
> until CLI install lands (Phase C). Catalog search works today; one-click
> install / dashboard browser remain phased (below).

### Out of scope for the host-rename / external-default PR

The following are **follow-up work**, not part of the Phase 1–2 host/packaging
landing:

- `bookclerk plugins install` / `update` / `remove` and install receipts
- Federated discovery beyond crates.io search (npm, PyPI, static registries)
- Windows AppContainer spawn-time confinement
- Publisher code signing / notarization as a hard requirement
- TypeScript / Python guest SDKs and packaging templates

## Design principle

| Layer | Role |
| --- | --- |
| crates.io crate | **Discovery index** + install URL metadata for plugin *authors* |
| HTTPS downloadable archive | **Prebuilt binaries** for each OS/arch (any host) |
| `$BOOKCLERK_FILES_DIR/plugins/<id>/` | **Installed** layout Bookclerk already loads |
| Platform installer / `cargo package-platform` | **Hosts + sqlite + local** — always bundled, not pulled from crates.io |

Bookclerk never runs `cargo build` on the user’s machine. Installing a plugin
downloads a release asset over HTTPS, verifies it, and unpacks `plugin.toml` +
binary — the same layout as a manual drop-in. The asset host is **not** tied to
GitHub: S3/R2, GitLab/Forgejo/Codeberg releases, a CDN, or a self-hosted static
directory all work as long as the URL is a direct download.

**Automatic pull today:** nothing from crates.io. Platform plugins ship inside
the host installer. Optional storefronts install later via
`bookclerk plugins install` (planned) or manual archive unpack.

See [packaging.md](packaging.md) for `cargo package-*` aliases and GitHub Actions
signing notes.

```text
crates.io ──search / metadata──► bookclerk plugins search|install
                                        │
                                        ▼
                              HTTPS archive URL
                              (any host; per OS/arch)
                                        │
                                        ▼
                              plugins/<id>/{plugin.toml, binary}
```

## Crate naming taxonomy

Publish under a **predictable crate name**:

```text
bookclerk-plugin-{kind}-{id}
```

| Segment | Rules | Examples |
| --- | --- | --- |
| Prefix | Always `bookclerk-plugin-` | — |
| `{kind}` | One of `source`, `integration`, `output`, `database` | `source` |
| `{id}` | Plugin id: lowercase `a-z`, `0-9`, `_` only; 2–32 chars; must match `plugin.toml` `id` and `[sources.<id>]` / … | `spotify`, `my_store` |

**Examples**

| Crate | Kind | Plugin id |
| --- | --- | --- |
| `bookclerk-plugin-source-spotify` | source | `spotify` |
| `bookclerk-plugin-integration-plex` | integration | `plex` |
| `bookclerk-plugin-output-webdav` | output | `webdav` |
| `bookclerk-plugin-database-libsql` | database | `libsql` |

**Binary name:** prefer the same string as the crate name
(`bookclerk-plugin-source-spotify`), referenced from `plugin.toml` `command`.

First-party external plugins (`bookclerk-plugin-source-audible`,
`bookclerk-plugin-source-libro`, `bookclerk-plugin-integration-audiobookshelf`,
…) keep `publish = false` until intentionally released; they are built and
staged in CI only. The echo example likewise stays unpublished as a template.
Workspace hosts may still `register()` the same adapters in-process for Rust
development.

### crates.io keywords & categories

Every published plugin crate **must** include:

```toml
[package]
name = "bookclerk-plugin-source-example"
keywords = ["bookclerk", "bookclerk-plugin", "audiobook"]
# optional kind keyword for filtering:
# keywords = ["bookclerk", "bookclerk-plugin", "bookclerk-source", "audiobook"]
categories = ["multimedia", "command-line-utilities"]
```

Kind-specific keyword (optional but recommended): `bookclerk-source`,
`bookclerk-integration`, `bookclerk-output`, `bookclerk-database`.

Discovery queries:

1. Prefer crates whose name starts with `bookclerk-plugin-`
2. And/or keyword `bookclerk-plugin`
3. Then parse `{kind}` / `{id}` from the name (authoritative over loose keywords)

## Cargo package metadata

Authors declare install metadata in `Cargo.toml` so the host does not need a
git clone to know where binaries live. Artifact fields are **plain HTTPS URL
templates** — any host that serves a direct download is fine.

```toml
[package.metadata.bookclerk]
api_version = 1
kind = "source"                 # must match name segment
id = "example"                  # must match name segment + plugin.toml
display_name = "Example Store"

# Option A — directory/prefix + conventional filenames (most hosts):
# Placeholders: {tag} {version} {target} {crate} {ext}
artifact_base_url = "https://cdn.example.com/bookclerk-plugins/{crate}/{version}"
# → {artifact_base_url}/{crate}-{version}-{target}.{ext}

# Option B — full URL template when the host path layout differs:
# artifact_url = "https://downloads.example.com/v/{tag}/{crate}-{target}.{ext}"

# Examples of hosts (all equivalent as long as GET returns the archive bytes):
# artifact_base_url = "https://github.com/org/repo/releases/download/{tag}"
# artifact_base_url = "https://gitlab.com/org/repo/-/releases/{tag}/downloads"
# artifact_base_url = "https://codeberg.org/org/repo/releases/download/{tag}"
# artifact_base_url = "https://my-bucket.s3.amazonaws.com/plugins/{crate}"
# artifact_base_url = "https://pub-….r2.dev/bookclerk/{crate}/{version}"

# Optional: path inside the archive to the plugin root (default: ".")
# archive_root = "."
# Optional: min Bookclerk host version (semver req), when enforced
# min_host = "0.1.0"
```

Prefer **Option A** when your files follow the recommended names below. Use
**Option B** (`artifact_url`) when the object key cannot be expressed as
`{base}/{crate}-{version}-{target}.{ext}`.

The crate’s `readme` / crate description should summarize trust/scope (what
accounts it talks to). Enabling still means running that binary, jailed — see the
trust model in [plugins.md](plugins.md).

## Artifact naming (install without Rust)

Release each version as a **per-target archive** (`.tar.gz` on Unix, `.zip` on
Windows) containing at least:

```text
plugin.toml
bookclerk-plugin-{kind}-{id}    # or .exe on Windows
```

Two consequences of [the guest jail](plugins.md#the-guest-jail) for what an
archive may assume. The install directory is **read-only** at runtime, so a
plugin that wants to keep state must use the `plugin_data_dir` it is given (also
its `HOME`) or `TMPDIR` — not a path beside its own binary. And a plugin that
needs more than outbound HTTPS has to say so in `plugin.toml`:

```toml
[sandbox]
network = "listen"   # only for an OAuth callback on loopback
```

Recommended asset names (cargo-dist / cargo-binstall friendly):

```text
{crate}-{version}-{target}.tar.gz
{crate}-{version}-{target}.zip
```

Examples:

```text
bookclerk-plugin-source-example-0.1.0-x86_64-unknown-linux-gnu.tar.gz
bookclerk-plugin-source-example-0.1.0-aarch64-apple-darwin.tar.gz
bookclerk-plugin-source-example-0.1.0-x86_64-pc-windows-msvc.zip
```

With Option A, `{ext}` is `tar.gz` or `zip` (Windows targets). `{tag}` is
usually `v{version}` when the host uses git-style tags; hosts that key only by
version can omit `{tag}` from the template.

The install client issues a plain `GET` (following redirects). No GitHub,
GitLab, or cloud API tokens are required for public assets. Private/authenticated
buckets are out of scope for v1 (operators can still unpack manually).

**Checksums:** publish `SHA256SUMS` (or per-asset `.sha256`) next to assets, or
embed digests in a curated index later. Future install will require a matching
digest before enabling by default.

**Signing (later):** optional minisign/cosign; dashboard can surface “signed by
publisher” vs “crates.io metadata only”.

## Host install layout

`bookclerk plugins install <crate-or-id>` (planned) should:

1. Resolve crate via crates.io (name taxonomy or exact crate)
2. Read `[package.metadata.bookclerk]` from the crate’s published Cargo.toml
3. Pick the host `target_triple` (or override)
4. Download + verify the archive
5. Extract into `$BOOKCLERK_FILES_DIR/plugins/<id>/`
6. Leave `enabled = false` for integrations (existing default); operator enables
   in config / dashboard

No `cargo` / `rustc` on PATH is required.

## Operator dashboard (phased)

| Phase | What |
| --- | --- |
| **A — Taxonomy (this doc + types)** | Stable crate names, metadata schema, validation helpers |
| **B — CLI catalog** | `bookclerk plugins search [query]` against crates.io; `info` shows registry metadata |
| **C — CLI install** | Download/verify/unpack; `plugins update` / `remove` |
| **D — Daemon API** | Authenticated `/api/plugins/catalog`, `/api/plugins/install` |
| **E — Dashboard browser** | Browse / configure / enable in the operator SPA |

Phases D–E reuse the same catalog DTOs as the CLI. Config knobs remain in
`config.toml` tables; the UI edits those settings after install.

## Curated index (optional later)

crates.io is a convenient global index but is not a security boundary. A
future **Bookclerk plugin index** (signed JSON, hosted by the project) can:

- Mirror only reviewed crates
- Pin artifact URLs + digests
- Power the dashboard “Featured” list

The naming taxonomy stays identical so community crates remain discoverable
even when not featured.

## Standalone plugin development (no Bookclerk mirror)

Third-party authors keep **their own repo**. They do **not** fork or vendor the
Bookclerk monorepo. The contract is the JSON-RPC protocol + install layout; the
host binary discovers whatever lands under `plugins/`.

### Guest SDK crate (`bookclerk-plugin-sdk`)

Use the slim **guest-only** crate — not the host `bookclerk-plugin-host` (that pulls
config/library/source and is for Bookclerk itself):

```toml
# In the author's standalone crate (their repo / workspace)
[package]
name = "bookclerk-plugin-source-example"

[dependencies]
# Path while developing against a local checkout:
bookclerk-plugin-sdk = { path = "../bookclerk/crates/bookclerk-plugin-sdk" }
# Or git (no crates.io publish required yet):
# bookclerk-plugin-sdk = { git = "https://github.com/fritz-fritz/bookclerk", package = "bookclerk-plugin-sdk" }

[package.metadata.bookclerk]
api_version = 1
kind = "source"
id = "example"
artifact_base_url = "https://cdn.example.com/…/{version}"
```

In-tree first-party plugins: `crates/bookclerk-plugins/source-{audible,libro,chirp,graphicaudio}`
(each package is lib + guest bin) and `crates/bookclerk-plugin-examples/echo-integration`.
CI builds those binaries and
stages them with `cargo stage-plugins` for host integration tests
(`BOOKCLERK_PLUGIN_ARTIFACTS`) — no public artifact release yet.

Cargo only builds the SDK’s small dependency graph (`serde`, `tokio` I/O,
`chrono`, …) — not the rest of the Bookclerk workspace.

Author loop:

1. New git repo with one binary crate named per the taxonomy
2. Depend on `bookclerk-plugin-sdk` (path or git)
3. Implement handshake / source|integration methods via `PluginGuest::serve`
4. CI builds release archives per target; upload wherever `artifact_*` points
5. Later: `cargo publish` the plugin crate (and eventually the SDK) for
   `bookclerk plugins search`
6. Operators install the **archive**, never clone your (or our) git tree

### Why a separate crate (not features / not `*-dev`)

| Option | Verdict |
| --- | --- |
| **`bookclerk-plugin-sdk`** (chosen) | Clear guest surface; host cannot leak into author builds; publishable later without renaming |
| Features on `bookclerk-plugin-host` (`bundled-plugins`) | Opt-in in-process dev; release hosts omit storefront features |
| `bookclerk-plugin-dev` | Sounds like build tooling; authors would think it’s test-only |

### What you never need

- A mirror of `fritz-fritz/bookclerk`
- Linking against `bookclerkd` / `bookclerk-cli` / host `bookclerk-plugin-host`
- Matching our workspace `Cargo.toml` beyond the SDK’s MSRV
- Rust at all (Go/Python/Node/… binary that speaks the protocol is valid; crates.io
  is then optional discovery sugar)
- crates.io today (path/git SDK is enough)

## Non-goals

- Compiling plugins from crates.io source on the operator machine
- Loading `cdylib` / WASM into the host process (subprocess RPC stays the ABI)
- Requiring plugin authors to use Rust (any language that ships the archive +
  speaks JSON-RPC is fine; crates.io is optional for non-Rust publishers — they
  can still ship archives and be listed on a curated index)
- Requiring authors to fork or mirror the Bookclerk monorepo

Non-Rust publishers: use the same `plugin.toml` + asset naming; omit the crate
or publish a thin “manifest-only” crate that only carries
`[package.metadata.bookclerk]` and documentation.

## Validation checklist for publishers

- [ ] Own standalone repo (no Bookclerk mirror required)
- [ ] Crate name = `bookclerk-plugin-{kind}-{id}`
- [ ] `keywords` include `bookclerk` and `bookclerk-plugin`
- [ ] `[package.metadata.bookclerk]` `kind` / `id` / `api_version` match the name and `plugin.toml`
- [ ] Release assets for each supported target with checksums
- [ ] `plugin.toml` `command` points at the binary inside the archive
- [ ] `[sandbox] network` declared when outbound-only is not enough; state kept in
      `plugin_data_dir` / `TMPDIR`, never beside the binary
- [ ] Document required config keys and any password env vars

## Related

- Runtime discovery & protocol: [plugins.md](plugins.md)
- Architecture overview: [architecture.md](architecture.md)
- Operator GUI surfaces: [gui.md](gui.md) (plugin browser = future)
