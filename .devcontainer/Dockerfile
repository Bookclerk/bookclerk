# Dev Container image for Bookclerk.
#
# The IDE bind-mounts the repo into the container — do not COPY sources here.
# Build artifacts land in the workspace `target/` and `ui/dist/` so you can run
# the Linux binaries on the host (glibc ≥ bookworm).

FROM rust:1-bookworm

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        git \
        gnupg \
        jq \
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

# rust-toolchain.toml (stable + rustfmt/clippy) is honored on first cargo use;
# install components up front so the first `cargo check` is faster.
RUN rustup component add rustfmt clippy

# Create a non-root dev user. VS Code's `updateRemoteUserUID` remaps this
# account to the host UID/GID at container create time (no hardcoded host IDs).
RUN groupadd --gid 1000 bookclerk \
    && useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash bookclerk

ENV CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1

RUN mkdir -p /home/bookclerk/.cargo/registry /home/bookclerk/.cargo/git \
    && chown -R bookclerk:bookclerk /home/bookclerk/.cargo

WORKDIR /workspaces/bookclerk
