# Dev Container

Bookclerk’s Dev Container gives a controlled Linux build environment (Debian
Bookworm + Rust + OpenSSL headers + Node) so local hosts without `libssl-dev`
(or with mismatched toolchains) can still compile.

The same Dockerfile is shared with Cursor Cloud Agents
([`.devcontainer/Dockerfile`](../.devcontainer/Dockerfile), referenced from
[`.cursor/environment.json`](../.cursor/environment.json)). The image must
**not** set `WORKDIR /workspace` or bake `CARGO_*` / `TMPDIR` under
`/workspace` — Cursor clones the repo there at runtime; workspace cache paths
are set by Dev Container `remoteEnv` or
[`.cursor/cloud-agent-install.sh`](../.cursor/cloud-agent-install.sh).

When **Environment Builds** are enabled in the Cursor dashboard, new agents
boot only from a **healthy SYSTEM/RECURRING** build of the default branch.
Agent-triggered draft builds never become that baseline. If Builds is on and
the Builds tab shows `no_healthy_builds` (or only failed SYSTEM / draft-only
successes), disable Builds until a green default-branch SYSTEM build exists —
otherwise new agents fall into a long cold install and can appear stuck
without a usable checkout/branch.

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
the default `devcontainer.json` omits desktop mounts so headless hosts can start
cleanly.

Fallback: run the bind-mounted binaries on the host itself (glibc ≥ Bookworm):

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

## Open in Cursor / VS Code

1. Install the **Dev Containers** extension (Cursor: same Marketplace id as VS Code).
2. Command Palette → **Dev Containers: Reopen in Container**.
3. Wait for the image build and `postCreateCommand` (`cargo fetch`, `ui` `npm ci`).

Definition: [`.devcontainer/`](../.devcontainer/).

## What is installed

| Piece | Notes |
| --- | --- |
| `rust:1.85-bookworm` | Shared with Cloud Agents; `rust-toolchain.toml` selects `stable` + rustfmt/clippy |
| `pkg-config` + `libssl-dev` | Fixes the local OpenSSL / `openssl-sys` failure mode |
| `libdbus-1-dev` + `xdg-utils` | Linux tray (`ksni` / zbus) and `xdg-open` for “Open Bookclerk” |
| `openssh-client` | SSH commit signing in the Dev Container |
| Node.js 22 | `ui/` Vite build |
| Cargo registry/git | Workspace `.cargo-home/` (`CARGO_HOME` on the bind mount) |
| Linux desktop + tray | Optional [`devcontainer.linux-desktop.json`](../.devcontainer/devcontainer.linux-desktop.json) |
| `PATH` | `target/debug` then `target/release` (via `remoteEnv`) |

## SSH commit signing (Dev Container)

**Typical setup is ordinary host `git config`** — not environment variables.
Cursor/VS Code copies your host `~/.gitconfig` into the Dev Container. Cloud
Agent Builds never run the Dev Container `postStart` helper and use a different
home directory (`/root`) plus Cursor’s HSM wiring, so Dev Container overlays
cannot clobber Cloud signing.

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

Then reopen the Dev Container. [`git-signing.sh`](../.devcontainer/git-signing.sh)
only ensures `SSH_AUTH_SOCK` reaches a usable agent (`ssh-add -l` succeeds) and
applies project-local (`.git/config`) tweaks when needed (e.g. host
`user.signingkey` path missing in the container). It never rewrites Cloud’s
global HSM gitconfig.

### Optional env overrides

`remoteEnv` still forwards these from the host when set (empty = ignored). They
are applied with `git config --local` (repo `.git/config`), not Cloud global:

| Host env var | Purpose |
| --- | --- |
| `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` | Override `user.name` / `user.email` |
| `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL` | Fallback identity if author unset |
| `GIT_SSH_SIGNING_PUBKEY` | Pin `user.signingkey` (`key::` added if missing) |

### Agent socket sources

| Source | How the agent reaches the container |
| --- | --- |
| Default `devcontainer.json` | Cursor/VS Code SSH-agent forwarder when it works |
| Linux desktop config | Bind-mounts host `${SSH_AUTH_SOCK}` → `/run/host-services/ssh-auth.sock` |
| macOS Docker Desktop (optional) | Magic `/run/host-services/ssh-auth.sock` — mount if the IDE forwarder is empty |

Cloud Agents keep Cursor HSM (`gpg.ssh.program=cursor-git-ssh-keygen`); the
helper refuses to modify config when that is present. Do not put private keys
in the image, `remoteEnv`, or the repo.

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

Mirrors [AGENTS.md](../AGENTS.md) / CI. For packaging the release image, see
[packaging.md](packaging.md) (`packaging/docker/Dockerfile`) — that path is
separate from this Dev Container.
