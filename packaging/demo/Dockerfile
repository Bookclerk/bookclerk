# Hosted demo image: the production image plus the four storefront plugins
# and the covers plugin, minus everything that could acquire a book. Built by
# .github/workflows/demo-deploy.yml (through `wrangler deploy`) on top of two
# images it builds first from the same commit with
# packaging/docker/Dockerfile, so every crate and plugin in the demo is the
# version production ships:
#
#   docker build -f packaging/docker/Dockerfile --target builder -t bookclerk-builder:demo .
#   docker build -f packaging/docker/Dockerfile -t bookclerk:demo .
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

ARG BUILDER=bookclerk-builder:demo
ARG RUNTIME=bookclerk:demo

FROM ${BUILDER} AS storefronts
WORKDIR /src
# Reuses the release build the production image already made.
RUN cargo stage-plugins --release --plugin audible --plugin libro --plugin chirp --plugin graphicaudio --plugin covers \
    && mkdir -p /out \
    && cp -a target/plugin-artifacts/. /out/ \
    && test "$(find /out -mindepth 2 -maxdepth 2 -name plugin.toml | wc -l)" -eq 5 \
    && cargo build --release -p bookclerk-demo-seed \
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
