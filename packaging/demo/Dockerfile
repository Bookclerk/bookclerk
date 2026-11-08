# Hosted demo image: the production image plus the four storefront plugins
# and the covers plugin, minus everything that could acquire a book. Built on
# the production builder and runtime stages of packaging/docker/Dockerfile
# from the same commit, so every crate and plugin in the demo is the version
# production ships, in one BuildKit build that never exports the builder:
#
#   docker buildx bake -f packaging/demo/docker-bake.hcl --load demo
#
# (from the repository root; the result is tagged bookclerk-demo:local).
# images-publish.yml and demo-deploy.yml both build it this way. The bake
# file builds the demo's binaries with thin LTO and parallel codegen units
# rather than the fat LTO shipped images use, which links far faster.
#
# The build context is this directory; the workflow writes version.txt (the
# commit) into it.
#
# What differs from production, and why:
#   - The storefronts (Audible, Libro.fm, Chirp, GraphicAudio) and the
#     covers plugin are staged from the production builder, with their
#     shipped network grants. Covers, store icons and avatars are downloaded
#     through `covers` and re-encoded by bookclerk-media-worker, so the web
#     app can load them from its own origin.
#   - No `local` output plugin: with no destination deployed, the host
#     refuses acquire work by its own fail-closed rules. The demo Worker
#     refuses the acquire routes before that (tools/demo/src/routes.ts).
#   - plugins.isolation and media.isolation = "best-effort": Cloudflare's
#     container kernel has no Landlock, so the host could not jail any guest
#     or image job otherwise. Decided for the demo only: it runs first-party
#     plugins, visitors cannot add others, and the media worker only
#     processes images there, so each visitor's own VM is the boundary. The
#     jail still applies seccomp, each guest's network policy and the media
#     worker's own process limits. Shipped builds keep "required".
#   - bookclerkd listens on all interfaces inside the VM, where only the
#     Worker reaches it.
#   - bookclerk-demo-seed runs once before bookclerkd: it adds a second
#     library user with books, a wishlist and an offline branch
#     (crates/bookclerk-demo-seed), so the library the visitor lands in is
#     not empty. It approves and deploys the storefronts (their manifest
#     grants, as the Worker does later), fetches each title's record from
#     its store's catalog through them, then writes through the library code
#     of this commit, never raw SQL. If it fails, bookclerkd still starts,
#     with an empty library.

# The bake file points these at the production Dockerfile's stages.
ARG BUILDER=bookclerk-builder:demo
ARG RUNTIME=bookclerk:demo

FROM ${BUILDER} AS storefronts
WORKDIR /src
# The link settings the builder stage got (see docker-bake.hcl).
ARG RELEASE_LTO=true
ARG RELEASE_CODEGEN_UNITS=1
# Reuses the release build the production image already made. The
# storefronts and the seed build in one cargo invocation, so the dependencies
# they share compile once, with one feature set; stage-plugins then only lays
# out what was built.
RUN CARGO_PROFILE_RELEASE_LTO="$RELEASE_LTO" \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS="$RELEASE_CODEGEN_UNITS" \
    cargo build --release \
      -p bookclerk-plugin-source-audible \
      -p bookclerk-plugin-source-libro \
      -p bookclerk-plugin-source-chirp \
      -p bookclerk-plugin-source-graphicaudio \
      -p bookclerk-demo-seed \
    && cargo stage-plugins --release --skip-build --plugin audible --plugin libro --plugin chirp --plugin graphicaudio --plugin covers \
    && mkdir -p /out \
    && cp -a target/plugin-artifacts/. /out/ \
    && test "$(find /out -mindepth 2 -maxdepth 2 -name plugin.toml | wc -l)" -eq 5 \
    && install -m 0755 target/release/bookclerk-demo-seed /bookclerk-demo-seed

FROM ${RUNTIME}
USER root
RUN test -x /opt/bookclerk/bookclerk-media-worker \
    && for manifest in /opt/bookclerk/plugins/*/plugin.toml; do \
         if grep -qx 'id = "local"' "$manifest"; then rm -rf "$(dirname "$manifest")"; fi; \
       done \
    && ! grep -rqx 'id = "local"' /opt/bookclerk/plugins \
    && grep -rqx 'id = "sqlite"' /opt/bookclerk/plugins
COPY --from=storefronts /out/ /opt/bookclerk/storefronts/
COPY --from=storefronts /bookclerk-demo-seed /opt/bookclerk/bookclerk-demo-seed
COPY version.txt /opt/bookclerk/DEMO_VERSION
ENV BOOKCLERK_PLUGIN_DIRS=/opt/bookclerk/storefronts \
    BOOKCLERK_PLUGIN_ISOLATION=best-effort \
    BOOKCLERK_MEDIA_ISOLATION=best-effort \
    BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787
USER bookclerk
# Seed, then hand the process over to bookclerkd. A seed failure is logged
# and the demo starts with an empty library rather than not at all.
ENTRYPOINT ["/bin/sh", "-c", "/opt/bookclerk/bookclerk-demo-seed || echo 'bookclerk-demo-seed failed; starting with an empty library' >&2; exec /opt/bookclerk/bookclerkd"]
