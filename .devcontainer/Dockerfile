# Dev Container base image for Bookclerk.
#
# The Dev Container bind-mounts the repo (do not COPY sources). The images are
# pinned by digest; Dependabot (docker ecosystem) bumps the FROM lines, the same
# rust and node images as packaging/docker/Dockerfile so the two stay aligned.
#
# Do NOT set WORKDIR or ENV paths under the workspace here: the image must not
# own that tree. Dev Containers use ${containerWorkspaceFolder}; workspace
# caches (target/, .cargo-home/, BookclerkFiles/) are created by postCreate
# and pointed at via remoteEnv.

# Node.js for `ui/` and the Workers under tools/: taken from the official image
# rather than an apt repository so Dependabot tracks the version.
FROM node:26-bookworm-slim@sha256:28147155d47dc31abe0a46e0d12b59b2fb3d0a2dcc5dc65fce76a074180b16fe AS node

# uv installs the Python plugin SDK's dev/docs tools from its uv.lock (the
# `python_sdk` CI check and `generate-api-docs.sh --python`). Same version as
# CI's setup-uv step. A FROM stage rather than a bare COPY --from so Dependabot
# sees it.
FROM ghcr.io/astral-sh/uv:0.11.32@sha256:df4cae8f3a96d175e2e5f992e597550000edbe78fdc2594d5cd8de1a217f504c AS uv

FROM rust:1-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        capnproto \
        clang \
        cmake \
        curl \
        git \
        jq \
        openssh-client \
        pkg-config \
        libssl-dev \
        libdbus-1-dev \
        # Tray "Open Bookclerk" uses open::that → xdg-open (host portal over session bus).
        xdg-utils \
        # Handy when debugging listen / tray / HTTP from inside the container.
        iproute2 \
        procps \
    && rm -rf /var/lib/apt/lists/*

# Node.js binary and the bundled npm from the `node` stage above, with npm and
# npx linked the way the official image links them.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

COPY --from=uv /uv /uvx /usr/local/bin/

# rust-toolchain.toml (stable + rustfmt/clippy) is honored on first cargo use;
# install components up front so the first `cargo check` is faster.
RUN rustup component add rustfmt clippy

# Some non-login shells start with a minimal PATH that omits
# /usr/local/cargo/bin. Symlink the toolchain into /usr/local/bin so
# `cargo`/`rustc` remain usable without a manual PATH export.
RUN for b in cargo rustc rustup rustfmt rustdoc cargo-clippy clippy-driver cargo-fmt; do \
      ln -sf "/usr/local/cargo/bin/$b" "/usr/local/bin/$b"; \
    done

# Create a non-root dev user. The Dev Containers `updateRemoteUserUID` setting
# remaps this account to the host UID/GID at container create time (no
# hardcoded host IDs).
RUN groupadd --gid 1000 bookclerk \
    && useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash bookclerk \
    && mkdir -p /home/bookclerk/.cargo/registry /home/bookclerk/.cargo/git \
    && chown -R bookclerk:bookclerk /home/bookclerk/.cargo

# Toolchain PATH only — workspace-local Cargo paths are set at runtime by the
# Dev Container remoteEnv, never baked into the image.
ENV PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1
