# Dev Container

Bookclerk’s Dev Container gives a controlled Linux build environment (Debian
13 trixie + Rust + OpenSSL headers + Node) so local hosts without `libssl-dev`
(or with mismatched toolchains) can still compile.

The image is [`.devcontainer/Dockerfile`](../../.devcontainer/Dockerfile). It must
**not** set `WORKDIR` or bake `CARGO_*` paths under the workspace; workspace
cache paths are set by the Dev Container `remoteEnv`.

The workspace is **bind-mounted**. Cargo writes into the repo’s `target/` and
`.cargo-home/` (registry/git), and Vite into `ui/dist/`. `target/debug` and
`target/release` are prepended to `PATH`, and `BOOKCLERK_FILES_DIR` defaults to
`<workspace>/BookclerkFiles`, so after a build you can exercise the CLI
in-container:

```bash
cargo build -p bookclerk-cli -p bookclerkd -p bookclerk-jail -p bookclerk-media-worker
bookclerk version
cargo stage-plugins   # or: cargo dev / cargo dev-cli -- …
```

On a **Linux desktop host**, reopen with the alternate config
[`.devcontainer/devcontainer.linux-desktop.json`](../../.devcontainer/devcontainer.linux-desktop.json)
(Command Palette → **Dev Containers: Reopen in Container…** and pick that file, or
set `"name"` / open via the config picker). That variant bind-mounts the host
session runtime (`$XDG_RUNTIME_DIR` → `/run/host-user`) and `/tmp/.X11-unix`, and
sets `DBUS_SESSION_BUS_ADDRESS` so `bookclerkd`’s in-process tray (`ksni`) can
appear on the **host** StatusNotifier panel. It also relaxes AppArmor/SELinux
labels for Landlock/sandbox experiments — keep those `runArgs` out of the
default portable config.

Docker fails container create if a bind `source` is missing or
`${localEnv:XDG_RUNTIME_DIR}` is empty — it does not skip the mount. That is why
the default `devcontainer.json` omits desktop mounts so headless hosts can start
cleanly.

Fallback: run the bind-mounted binaries on the host itself (glibc ≥ 2.41, Debian 13 trixie or newer):

```bash
# inside the Dev Container
cargo build -p bookclerkd -p bookclerk-cli
cd ui && npm run build

# on the host (same checkout)
BOOKCLERK_FILES_DIR="$PWD/BookclerkFiles" \
BOOKCLERK_UI_DIST="$PWD/ui/dist" \
./target/debug/bookclerkd
```

Windows/macOS hosts can build here but cannot execute the resulting ELF binaries
natively.

## Open in your editor

1. Install your editor's **Dev Containers** support.
2. Command Palette → **Dev Containers: Reopen in Container**.
3. Wait for the image build and `postCreateCommand` (`cargo fetch`, `ui` `npm ci`).

Definition: [`.devcontainer/`](../../.devcontainer/). The container config sets
the rust-analyzer options the project expects (`rust-analyzer.check.command =
"clippy"`, `cargo.allFeatures = false`) and recommends extensions; the repo
commits only [`.vscode/extensions.json`](../../.vscode/extensions.json), never
editor settings.

## Building on the host without a container

Local host terminals use the default `~/.cargo`. Workspace `target/` and
`BookclerkFiles/` come from [`.cargo/config.toml`](../../.cargo/config.toml)
relative values, which Cargo discovers in every environment. To match the
editor setup outside the container, set rust-analyzer's check command to
`clippy` in your own settings, and add `target/debug` to `PATH` if you want to
run host binaries without `cargo run`.

Workspace `.cargo-home/` is the Dev Container's `CARGO_HOME`, so the registry
survives image rebuilds when `$HOME` is inside the container (not the bind
mount). `.cargo/config.toml` cannot set `CARGO_HOME` for Cargo's own registry
(that variable is read before project config), so the container exports an
absolute workspace path via `remoteEnv`.

## What is installed

The Dockerfile takes Rust, Node.js and uv from their official images and runs
[`scripts/dev-env/provision.sh`](../../scripts/dev-env/provision.sh) for
everything else. That script is the one list of what a development
environment needs: add a tool there, not in the Dockerfile.

| Piece | Notes |
| --- | --- |
| `rust:1-trixie` (digest-pinned, Dependabot bumps it) | Base only: the provision script installs the `stable` toolchain `rust-toolchain.toml` names (with rustfmt/clippy), so the first cargo command downloads nothing |
| Node.js 26 (from the `node:26-trixie-slim` image, digest-pinned) | `ui/` Vite build and the Workers under `tools/` |
| uv (from its image, same version as CI) | Python plugin SDK dev and docs tools |
| `cargo-nextest` | Same version CI pins; `rust_test` runs under it |
| Python 3 + a virtualenv with `scripts/tests/requirements.txt` | First on `PATH`, so `python3` has PyYAML for the CI planner tests |
| `pkg-config` + `libssl-dev` | Fixes the local OpenSSL / `openssl-sys` failure mode |
| `capnproto` | Compiles `schema/plugin.capnp` (plugin ABI native RPC) |
| `libdbus-1-dev` + `xdg-utils` | Linux tray (`ksni` / zbus) and `xdg-open` for “Open Bookclerk” |
| `openssh-client` | SSH commit signing in the Dev Container |
| Cargo registry/git | Workspace `.cargo-home/` (`CARGO_HOME` on the bind mount) |
| Linux desktop + tray | Optional [`devcontainer.linux-desktop.json`](../../.devcontainer/devcontainer.linux-desktop.json) |
| `PATH` | `target/debug` then `target/release` (via `remoteEnv`) |

The **Dev Container image** CI workflow builds the image whenever these files
change and weekly, and checks each tool runs as the dev user
([CI jobs](ci.md#jobs)). The Linux desktop configuration must keep the default
configuration's shared keys verbatim and only add to them;
`scripts/tests/test_dev_env.py` enforces that.

## Hosted and disposable sessions

A hosted cloud workspace, or any throwaway Debian or Ubuntu machine where you
are root and rustup is installed, can use the Dev Container's toolchain without opening the
container, in one of two ways.

**Natively (default).** Run once per session, from the checkout:

```bash
scripts/dev-env/session-setup.sh
```

It copies Node.js and uv out of the same digest-pinned images the
Dockerfile uses (starting `dockerd` if needed, see
[`docker-up.sh`](../../scripts/dev-env/docker-up.sh)), then runs the
provision script for everything else, and writes
`/etc/profile.d/zz-bookclerk-dev.sh` so new shells find the tools first. A
failed step is reported and the rest still run, so the session starts
regardless. Only the packages the Node.js image bundles (npm, corepack) replace
their host copies; other global npm packages stay. The host's own system packages stay its own: on Ubuntu 24.04,
for example, `capnp` is 1.0.1 rather than the image's 1.1.0.

**Inside the real image**, for exact parity (a toolchain-sensitive failure, a
check before filing a bug):

```bash
scripts/dev-env/in-devcontainer.sh cargo test -p bookclerk-db-exec
scripts/dev-env/in-devcontainer.sh bash
```

The image is built on first use and rebuilt only when the Dockerfile, the
provision script or the test requirements change. The checkout is
bind-mounted with the host's `target/devcontainer/` mounted over its `target/`
(so `cargo dev` and the other helpers find their builds where they expect
them, apart from the host's), and the command runs as your UID.

Two variables adapt both to restricted networks, and the setup script records
them in the profile for later shells:

| Variable | Purpose |
| --- | --- |
| `BOOKCLERK_DEV_REGISTRY_MIRROR` | Registry mirror for a newly started `dockerd` (for example `https://mirror.gcr.io` when shared egress is rate-limited by Docker Hub). Images stay pinned by digest. |
| `BOOKCLERK_DEV_CA_BUNDLE` | Complete PEM trust bundle (system roots plus the CA of a TLS-intercepting proxy): passed to the image build as the `extra_ca` secret and mounted into the container. It replaces the default trust store for cargo, rustup and pip, so a bundle holding only the proxy's CA breaks hosts the proxy does not intercept. The setup script also points `SSL_CERT_FILE`, `CARGO_HTTP_CAINFO`, `PIP_CERT` and `NODE_EXTRA_CA_CERTS` at it in the profile. |

A hosted environment's setup script can call it directly, for example
`cd bookclerk && BOOKCLERK_DEV_REGISTRY_MIRROR=https://mirror.gcr.io scripts/dev-env/session-setup.sh`.

## SSH commit signing (Dev Container)

**Typical setup is ordinary host `git config`** — not environment variables.
Dev Containers support copies your host `~/.gitconfig` into the container.

### Host (authoritative)

```bash
eval "$(ssh-agent -s)"
ssh-add ~/.ssh/id_ed25519   # private key stays on the host / in the agent

git config --global user.name "Your Name"
git config --global user.email "you@example.com"
git config --global gpg.format ssh
# Prefer key::… (or a .pub path). File paths are fixed up inside the container.
git config --global user.signingkey "key::$(ssh-add -L | head -1)"
git config --global commit.gpgsign true
```

Then reopen the Dev Container. [`git-signing.sh`](../../.devcontainer/git-signing.sh)
only ensures `SSH_AUTH_SOCK` reaches a usable agent (`ssh-add -l` succeeds) and
applies project-local (`.git/config`) tweaks when needed (e.g. host
`user.signingkey` path missing in the container). It never rewrites the global
gitconfig.

### Optional env overrides

`remoteEnv` still forwards these from the host when set (empty = ignored). They
are applied with `git config --local` (repo `.git/config`), not globally:

| Host env var | Purpose |
| --- | --- |
| `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` | Override `user.name` / `user.email` |
| `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL` | Fallback identity if author unset |
| `GIT_SSH_SIGNING_PUBKEY` | Pin `user.signingkey` (`key::` added if missing) |

### Agent socket sources

| Source | How the agent reaches the container |
| --- | --- |
| Default `devcontainer.json` | The editor's SSH-agent forwarder when it works |
| Linux desktop config | Bind-mounts host `${SSH_AUTH_SOCK}` → `/run/host-services/ssh-auth.sock` |
| macOS Docker Desktop (optional) | Magic `/run/host-services/ssh-auth.sock` — mount if the IDE forwarder is empty |

Do not put private keys in the image, `remoteEnv`, or the repo.

## Common commands (in the container)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p bookclerk-cli -p bookclerkd -p bookclerk-jail -p bookclerk-media-worker
bookclerk version
cd ui && npm run build
cargo dev
```

Mirrors [AGENTS.md](../../AGENTS.md) / CI. For packaging the release image, see
[packaging.md](packaging.md) (`packaging/docker/Dockerfile`) — that path is
separate from this Dev Container.
