# Dev Container

This page explains Bookclerk's Dev Container: a controlled Linux build
environment for hosts that can't build Bookclerk directly, how to open it,
what it installs, and how commit signing works inside it.

## What it is

The Dev Container is Debian 13 (trixie) with Rust, OpenSSL headers and
Node.js. Hosts without `libssl-dev`, or with a mismatched toolchain, can
still compile in it.

The image is [`.devcontainer/Dockerfile`](../../.devcontainer/Dockerfile).

> [!IMPORTANT]
> The image must **not** set `WORKDIR` or bake `CARGO_*` paths under the
> workspace. The Dev Container `remoteEnv` sets the workspace cache paths.

The workspace is **bind-mounted**:

- Cargo writes into the repository's `target/` and `.cargo-home/`
  (registry and git), and Vite into `ui/dist/`.
- `target/debug` and `target/release` are prepended to `PATH`.
- `BOOKCLERK_FILES_DIR` defaults to `<workspace>/BookclerkFiles`.

So after a build you can use the CLI inside the container:

```sh
cargo build -p bookclerk-cli -p bookclerkd -p bookclerk-jail -p bookclerk-media-worker
bookclerk version
cargo stage-plugins   # or: cargo dev / cargo dev-cli -- …
```

## Open in your editor

1. Install your editor's **Dev Containers** support.
2. In the Command Palette, run **Dev Containers: Reopen in Container**.
3. Wait for the image build and the `postCreateCommand` (`cargo fetch`,
   and `npm ci` in `ui`).

The definition is in [`.devcontainer/`](../../.devcontainer/). The
container config sets the rust-analyzer options the project expects
(`rust-analyzer.check.command = "clippy"`, `cargo.allFeatures = false`)
and recommends extensions. The repository commits only
[`.vscode/extensions.json`](../../.vscode/extensions.json), never editor
settings.

### On a Linux desktop

On a **Linux desktop host**, reopen with the alternate config
[`.devcontainer/devcontainer.linux-desktop.json`](../../.devcontainer/devcontainer.linux-desktop.json).
In the Command Palette, choose **Dev Containers: Reopen in Container…** and
pick that file (or set `"name"`, or open it through the config picker).

That variant:

- bind-mounts the host session runtime (`$XDG_RUNTIME_DIR` →
  `/run/host-user`) and `/tmp/.X11-unix`;
- sets `DBUS_SESSION_BUS_ADDRESS`, so `bookclerkd`'s in-process tray
  (`ksni`) can appear on the **host** StatusNotifier panel;
- relaxes AppArmor and SELinux labels for Landlock and sandbox
  experiments. Keep those `runArgs` out of the default portable config.

> [!NOTE]
> Docker fails to create the container if a bind `source` is missing or
> `${localEnv:XDG_RUNTIME_DIR}` is empty; it doesn't skip the mount. That
> is why the default `devcontainer.json` leaves out the desktop mounts, so
> headless hosts can start cleanly.

### Run the binaries on the host

As a fallback, run the bind-mounted binaries on the host itself. The host
needs glibc 2.41 or newer (Debian 13 trixie or newer).

1. Inside the Dev Container, build:

   ```sh
   cargo build -p bookclerkd -p bookclerk-cli
   cd ui
   npm run build
   cd ..
   ```

2. On the host, in the same checkout, run:

   ```sh
   BOOKCLERK_FILES_DIR="$PWD/BookclerkFiles" \
   BOOKCLERK_UI_DIST="$PWD/ui/dist" \
   ./target/debug/bookclerkd
   ```

Windows and macOS hosts can build here, but can't run the resulting ELF
binaries natively.

## Building on the host without a container

Local host terminals use the default `~/.cargo`. The workspace `target/`
and `BookclerkFiles/` come from relative values in
[`.cargo/config.toml`](../../.cargo/config.toml), which Cargo finds in
every environment.

To match the editor setup outside the container:

- set rust-analyzer's check command to `clippy` in your own settings;
- add `target/debug` to `PATH` if you want to run host binaries without
  `cargo run`.

The workspace `.cargo-home/` is the Dev Container's `CARGO_HOME`, so the
registry survives image rebuilds when `$HOME` is inside the container (not
the bind mount). `.cargo/config.toml` can't set `CARGO_HOME` for Cargo's
own registry, because that variable is read before project config. So the
container exports an absolute workspace path through `remoteEnv`.

## What is installed

The Dockerfile takes Rust, Node.js and uv from their official images, and
runs [`scripts/dev-env/provision.sh`](../../scripts/dev-env/provision.sh)
for everything else.

> [!IMPORTANT]
> That script is the one list of what a development environment needs.
> Add a tool there, not in the Dockerfile.

| Piece | Notes |
| --- | --- |
| `rust:1-trixie` (digest-pinned, bumped by Dependabot) | The base image only (see below) |
| Node.js 26 (from the digest-pinned `node:26-trixie-slim` image) | The `ui/` Vite build and the Workers under `tools/` |
| uv (from its image, same version as CI) | Python plugin SDK development and docs tools |
| `cargo-nextest` | The same version CI pins; `rust_test` runs under it |
| Python 3 + a virtualenv with `scripts/tests/requirements.txt` | First on `PATH`, so `python3` has PyYAML for the CI planner tests |
| `pkg-config` + `libssl-dev` | Fixes the local OpenSSL / `openssl-sys` failure |
| `capnproto` | Compiles `schema/plugin.capnp` (the plugin ABI's native RPC) |
| `libdbus-1-dev` + `xdg-utils` | The Linux tray (`ksni` / zbus), and `xdg-open` for "Open Bookclerk" |
| `openssh-client` | SSH commit signing in the Dev Container |
| Cargo registry and git | The workspace `.cargo-home/` (`CARGO_HOME` on the bind mount) |
| Linux desktop + tray | The optional [`devcontainer.linux-desktop.json`](../../.devcontainer/devcontainer.linux-desktop.json) |
| `PATH` | `target/debug`, then `target/release` (through `remoteEnv`) |

The provision script installs the `stable` toolchain that
`rust-toolchain.toml` names (with rustfmt and clippy) on top of the base
image, so the first cargo command doesn't download a toolchain.

The **Dev Container image** CI workflow builds the image whenever these
files change, and weekly. It checks that each tool runs as the dev user
(see [CI jobs](ci.md#jobs)).

The Linux desktop configuration must keep the default configuration's
shared keys word for word, and only add to them.
`scripts/tests/test_dev_env.py` enforces that.

## Hosted and disposable sessions

A hosted cloud workspace, or any throwaway Debian or Ubuntu machine where
you are root and rustup is installed, can use the Dev Container's toolchain
without opening the container. There are two ways.

### Natively (the default)

Run this once per session, from the checkout:

```sh
scripts/dev-env/session-setup.sh
```

It:

1. Copies Node.js and uv out of the same digest-pinned images the
   Dockerfile uses, starting `dockerd` if needed (see
   [`docker-up.sh`](../../scripts/dev-env/docker-up.sh)).
2. Runs the provision script for everything else.
3. Writes `/etc/profile.d/zz-bookclerk-dev.sh`, so new shells find the
   tools first.

A failed step is reported and the rest still run, so the session starts
regardless.

- Only the packages the Node.js image bundles (npm, corepack) replace
  their host copies. Other global npm packages stay.
- The host's own system packages stay its own. On Ubuntu 24.04, for
  example, `capnp` is 1.0.1 rather than the image's 1.1.0.

### Inside the real image

For exact parity (a toolchain-sensitive failure, or a check before filing
a bug), run commands inside the image:

```sh
scripts/dev-env/in-devcontainer.sh cargo test -p bookclerk-db-exec
scripts/dev-env/in-devcontainer.sh bash
```

- The image is built on first use, and rebuilt only when the Dockerfile,
  the provision script or the test requirements change.
- The checkout is bind-mounted, with the host's `target/devcontainer/`
  mounted over its `target/`. So `cargo dev` and the other helpers find
  their builds where they expect them, apart from the host's.
- The command runs as your UID.

### Restricted networks

Two variables adapt both ways to restricted networks. The setup script
records them in the profile for later shells.

| Variable | Purpose |
| --- | --- |
| `BOOKCLERK_DEV_REGISTRY_MIRROR` | A registry mirror for a newly started `dockerd` (see below) |
| `BOOKCLERK_DEV_CA_BUNDLE` | A complete PEM trust bundle: the system roots plus the CA of a TLS-intercepting proxy (see below) |

Use the mirror, for example `https://mirror.gcr.io`, when Docker Hub
rate-limits shared egress. Images stay pinned by digest.

`BOOKCLERK_DEV_CA_BUNDLE` is passed to the image build as the `extra_ca`
secret and mounted into the container. The setup script also points
`SSL_CERT_FILE`, `CARGO_HTTP_CAINFO`, `PIP_CERT` and
`NODE_EXTRA_CA_CERTS` at it in the profile.

> [!WARNING]
> The bundle replaces the default trust store for cargo, rustup and pip.
> A bundle holding only the proxy's CA breaks hosts the proxy doesn't
> intercept.

A hosted environment's setup script can run it before the session starts.
Such a script usually runs before the repository is cloned and must exit 0,
so it takes its own shallow clone:

```bash
#!/bin/bash
d=$(mktemp -d)
if git clone -q --depth 1 https://github.com/Bookclerk/bookclerk "$d"; then
  BOOKCLERK_DEV_REGISTRY_MIRROR=https://mirror.gcr.io "$d/scripts/dev-env/session-setup.sh" || true
fi
rm -rf "$d"
exit 0
```

The setup script appends everything it prints to
`/var/log/bookclerk-session-setup.log`, so a session that starts without a
tool can show why. It installs into root's toolchain whatever `HOME` the
hook passes.

## SSH commit signing (Dev Container)

The **usual setup is ordinary host `git config`**, not environment
variables. Dev Containers support copies your host `~/.gitconfig` into the
container.

### Host (authoritative)

1. Set up signing on the host:

   ```sh
   eval "$(ssh-agent -s)"
   ssh-add ~/.ssh/id_ed25519   # private key stays on the host / in the agent

   git config --global user.name "Your Name"
   git config --global user.email "you@example.com"
   git config --global gpg.format ssh
   # Prefer key::… (or a .pub path). File paths are fixed up inside the container.
   git config --global user.signingkey "key::$(ssh-add -L | head -1)"
   git config --global commit.gpgsign true
   ```

2. Reopen the Dev Container.

[`git-signing.sh`](../../.devcontainer/git-signing.sh) only makes sure
`SSH_AUTH_SOCK` reaches a usable agent (`ssh-add -l` succeeds). When
needed, it applies project-local (`.git/config`) tweaks, for example when
the host's `user.signingkey` path is missing in the container. It never
rewrites the global gitconfig.

### Optional environment overrides

`remoteEnv` still forwards these from the host when they are set (empty
values are ignored). They are applied with `git config --local` (the
repository's `.git/config`), not globally:

| Host variable | Purpose |
| --- | --- |
| `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` | Override `user.name` / `user.email` |
| `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL` | Fallback identity if the author is unset |
| `GIT_SSH_SIGNING_PUBKEY` | Pin `user.signingkey` (`key::` is added if missing) |

### Agent socket sources

| Source | How the agent reaches the container |
| --- | --- |
| Default `devcontainer.json` | The editor's SSH-agent forwarder, when it works |
| Linux desktop config | Bind-mounts the host `${SSH_AUTH_SOCK}` → `/run/host-services/ssh-auth.sock` |
| macOS Docker Desktop (optional) | The magic `/run/host-services/ssh-auth.sock`; mount it if the editor's forwarder is empty |

> [!WARNING]
> Don't put private keys in the image, in `remoteEnv` or in the
> repository.

## Common commands (in the container)

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p bookclerk-cli -p bookclerkd -p bookclerk-jail -p bookclerk-media-worker
bookclerk version
cd ui
npm run build
cd ..
cargo dev
```

These mirror [AGENTS.md](../../AGENTS.md) and CI.

## Next steps

- [Build from source and use the CLI](build-from-source.md): what to do
  once it builds.
- [Continuous integration](ci.md): the checks these commands mirror.
- [Release packaging](packaging.md): the release image
  (`packaging/docker/Dockerfile`), which is separate from this Dev
  Container.
