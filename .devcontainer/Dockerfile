# Shared Dev Container + Cursor Cloud Agent base image for Bookclerk.
#
# - Dev Container: bind-mounts the repo (do not COPY sources).
# - Cloud Agent: Cursor checks out the correct commit into /workspace.
# Pin to the same Rust minor as packaging/docker/Dockerfile.
#
# Do NOT set WORKDIR /workspace or ENV paths under /workspace here: the image
# must not own that tree. Cursor clones into /workspace at runtime; Dev
# Containers use ${containerWorkspaceFolder}. Workspace caches
# (target/, .cargo-home/, .tmp/, BookclerkFiles/) are created by install /
# postCreate and pointed at via remoteEnv / the Cloud install script.

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

# rust-toolchain.toml (stable + rustfmt/clippy) is honored on first cargo use;
# install components up front so the first `cargo check` is faster.
RUN rustup component add rustfmt clippy

# Cursor Cloud Agent shells often start with a minimal PATH that omits
# /usr/local/cargo/bin. Symlink the toolchain into /usr/local/bin so
# `cargo`/`rustc` remain usable without a manual PATH export.
RUN for b in cargo rustc rustup rustfmt rustdoc cargo-clippy clippy-driver cargo-fmt; do \
      ln -sf "/usr/local/cargo/bin/$b" "/usr/local/bin/$b"; \
    done

# Create a non-root dev user. VS Code/Cursor `updateRemoteUserUID` remaps this
# account to the host UID/GID at container create time (no hardcoded host IDs).
# Cloud Agents typically run as root and ignore this account.
RUN groupadd --gid 1000 bookclerk \
    && useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash bookclerk \
    && mkdir -p /home/bookclerk/.cargo/registry /home/bookclerk/.cargo/git \
    && chown -R bookclerk:bookclerk /home/bookclerk/.cargo

# Toolchain PATH only — workspace-local Cargo/TMPDIR paths are set at runtime
# (Cloud install script / Dev Container remoteEnv), never baked to /workspace.
ENV PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1
