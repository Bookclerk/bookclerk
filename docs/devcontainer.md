# Dev Container

Bookclerk’s Dev Container gives a controlled Linux build environment (Debian
Bookworm + Rust + OpenSSL headers + Node) so local hosts without `libssl-dev`
(or with mismatched toolchains) can still compile.

The workspace is **bind-mounted**. Cargo writes into the repo’s `target/` (and
Vite into `ui/dist/`). `target/debug` and `target/release` are prepended to
`PATH`, and `BOOKCLERK_FILES_DIR` defaults to `/tmp/BookclerkFiles`, so after a
build you can exercise the CLI in-container:

```bash
cargo build -p bookclerk-cli -p bookclerkd -p bookclerk-jail -p bookclerk-media-worker
bookclerk version
cargo stage-plugins   # or: cargo dev-daemon / cargo dev-cli -- …
```

On a **Linux desktop host**, reopen with the alternate config
[`.devcontainer/devcontainer.linux-desktop.json`](../.devcontainer/devcontainer.linux-desktop.json)
(Command Palette → **Dev Containers: Reopen in Container…** and pick that file, or
set `"name"` / open via the config picker). That variant bind-mounts the host
session runtime (`$XDG_RUNTIME_DIR` → `/run/host-user`) and `/tmp/.X11-unix`, and
sets `DBUS_SESSION_BUS_ADDRESS` so `bookclerkd`’s in-process tray (`ksni`) can
appear on the **host** StatusNotifier panel. It also relaxes AppArmor/SELinux
labels for Landlock/sandbox experiments — keep those `runArgs` out of the
default portable config.

Docker fails container create if a bind `source` is missing or
`${localEnv:XDG_RUNTIME_DIR}` is empty — it does not skip the mount. That is why
the default `devcontainer.json` omits desktop mounts so headless and
Cloud-agent hosts can start cleanly.

Fallback: run the bind-mounted binaries on the host itself (glibc ≥ Bookworm):

```bash
# inside the Dev Container
cargo build -p bookclerkd -p bookclerk-cli
cd ui && npm run build

# on the host (same checkout)
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles \
BOOKCLERK_UI_DIST="$PWD/ui/dist" \
./target/debug/bookclerkd
```

Windows/macOS hosts can build here but cannot execute the resulting ELF binaries
natively.

## Open in Cursor / VS Code

1. Install the **Dev Containers** extension (Cursor: same Marketplace id as VS Code).
2. Command Palette → **Dev Containers: Reopen in Container**.
3. Wait for the image build and `postCreateCommand` (`cargo fetch`, `ui` `npm ci`).

Definition: [`.devcontainer/`](../.devcontainer/).

## What is installed

| Piece | Notes |
| --- | --- |
| `rust:1-bookworm` | Base image; `rust-toolchain.toml` selects `stable` + rustfmt/clippy |
| `pkg-config` + `libssl-dev` | Fixes the local OpenSSL / `openssl-sys` failure mode |
| `libdbus-1-dev` + `xdg-utils` | Linux tray (`ksni` / zbus) and `xdg-open` for “Open Bookclerk” |
| Node.js 22 | `ui/` Vite build |
| Cargo registry/git volumes | Mounted under `$CARGO_HOME` (`/home/bookclerk/.cargo/{registry,git}`) |
| Linux desktop + tray | Optional [`devcontainer.linux-desktop.json`](../.devcontainer/devcontainer.linux-desktop.json) |
| `PATH` | `target/debug` then `target/release` (via `remoteEnv`) |

## Common commands (in the container)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p bookclerk-cli -p bookclerkd -p bookclerk-jail -p bookclerk-media-worker
bookclerk version
cd ui && npm run build
```

Mirrors [AGENTS.md](../AGENTS.md) / CI. For packaging the release image, see
[packaging.md](packaging.md) (`packaging/docker/Dockerfile`) — that path is
separate from this Dev Container.
