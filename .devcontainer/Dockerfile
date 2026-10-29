# Dev Container base image for Bookclerk.
#
# The Dev Container bind-mounts the repo, so do not COPY sources. The only
# files taken from the build context are scripts/dev-env/provision.sh and
# scripts/tests/requirements.txt (so .dockerignore must keep them). The images are
# pinned by digest; Dependabot (docker ecosystem) bumps the FROM lines, the same
# rust and node images as packaging/docker/Dockerfile so the two stay aligned.
# Debian 13 (trixie) throughout, matching the production image.
#
# Do NOT set WORKDIR or ENV paths under the workspace here: the image must not
# own that tree. Dev Containers use ${containerWorkspaceFolder}; workspace
# caches (target/, .cargo-home/, BookclerkFiles/) are created by postCreate
# and pointed at via remoteEnv.

# Node.js for `ui/` and the Workers under tools/: taken from the official image
# rather than an apt repository so Dependabot tracks the version.
FROM node:26-trixie-slim@sha256:36793bcbb160a6a16cede1e62b93f84e26b78a972a8a4c0b879c1c12f22761ec AS node

# uv installs the Python plugin SDK's dev/docs tools from its uv.lock (the
# `python_sdk` CI check and `generate-api-docs.sh --python`). Same version as
# CI's setup-uv step. A FROM stage rather than a bare COPY --from so Dependabot
# sees it.
FROM ghcr.io/astral-sh/uv:0.13.6@sha256:af22a52f5eafef3d78e551599d405c02df61919d18028f0cea8eda93bc9986d0 AS uv

FROM rust:1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc

# Node.js binary and the bundled npm from the `node` stage above, with npm and
# npx linked the way the official image links them.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

COPY --from=uv /uv /uvx /usr/local/bin/

# System packages, Rust components, cargo-nextest and the script-test
# virtualenv: scripts/dev-env/provision.sh is the one list of what a
# development environment needs, so add tools there, not here.
COPY scripts/dev-env/provision.sh /tmp/bookclerk-dev/scripts/dev-env/provision.sh
COPY scripts/tests/requirements.txt /tmp/bookclerk-dev/scripts/tests/requirements.txt
#
# An optional `extra_ca` build secret (a PEM bundle) is trusted for these
# downloads, for builds behind a TLS-intercepting proxy:
#   docker build --secret id=extra_ca,src=<bundle.pem> ...
# It is mounted for this step only and never stored in the image. The mount
# needs BuildKit, Docker's default builder since 23.0.
#
# The base image leaves RUSTUP_HOME and CARGO_HOME world-writable so the dev
# user can add components and targets; provision runs as root, so restore
# that for the toolchain it installs.
RUN --mount=type=secret,id=extra_ca \
    if [ -f /run/secrets/extra_ca ]; then export BOOKCLERK_DEV_CA_BUNDLE=/run/secrets/extra_ca; fi \
    && /tmp/bookclerk-dev/scripts/dev-env/provision.sh --repo /tmp/bookclerk-dev \
    && rm -rf /tmp/bookclerk-dev /var/lib/apt/lists/* /usr/local/cargo/registry \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"

# Some non-login shells start with a minimal PATH that omits
# /usr/local/cargo/bin. Symlink the toolchain into /usr/local/bin so
# `cargo`/`rustc` remain usable without a manual PATH export.
RUN for b in cargo rustc rustup rustfmt rustdoc cargo-clippy clippy-driver cargo-fmt cargo-nextest; do \
      ln -sf "/usr/local/cargo/bin/$b" "/usr/local/bin/$b"; \
    done

# Create a non-root dev user. The Dev Containers `updateRemoteUserUID` setting
# remaps this account to the host UID/GID at container create time (no
# hardcoded host IDs).
#
# Login shells (`bash -l`, the postCreate command) reset PATH in /etc/profile,
# so profile.d puts the script-test virtualenv and cargo back in front.
RUN printf '%s\n' 'export PATH="/opt/bookclerk-dev/venv/bin:/usr/local/cargo/bin:$PATH"' \
        > /etc/profile.d/bookclerk-dev.sh \
    && groupadd --gid 1000 bookclerk \
    && useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash bookclerk \
    && mkdir -p /home/bookclerk/.cargo/registry /home/bookclerk/.cargo/git \
    && chown -R bookclerk:bookclerk /home/bookclerk/.cargo

# Toolchain PATH only — workspace-local Cargo paths are set at runtime by the
# Dev Container remoteEnv, never baked into the image. The script-test
# virtualenv comes first so `python3` has PyYAML for scripts/tests.
ENV PATH=/opt/bookclerk-dev/venv/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1
