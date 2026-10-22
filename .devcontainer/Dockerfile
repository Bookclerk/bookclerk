# Dev Container base image for Bookclerk.
#
# The Dev Container bind-mounts the repo (do not COPY sources). Pin to the same
# Rust minor as packaging/docker/Dockerfile.
#
# Do NOT set WORKDIR or ENV paths under the workspace here: the image must not
# own that tree. Dev Containers use ${containerWorkspaceFolder}; workspace
# caches (target/, .cargo-home/, BookclerkFiles/) are created by postCreate
# and pointed at via remoteEnv.

FROM rust:1.85-bookworm

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
        gnupg \
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

# Node.js for `ui/` (Vite/React). Pin via NodeSource apt repo + keyring
# (no `curl | bash` setup script).
RUN mkdir -p /etc/apt/keyrings \
    && curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key \
        | gpg --dearmor -o /etc/apt/keyrings/nodesource.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/nodesource.gpg] https://deb.nodesource.com/node_22.x nodistro main" \
        > /etc/apt/sources.list.d/nodesource.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# uv installs the Python plugin SDK's dev/docs tools from its uv.lock (the
# `python_sdk` CI check and `generate-api-docs.sh --python`). Same version as
# CI's setup-uv step.
COPY --from=ghcr.io/astral-sh/uv:0.11.32 /uv /uvx /usr/local/bin/

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
