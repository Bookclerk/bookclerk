# The production image plus the Audiobookshelf integration plugin, for the
# Audiobookshelf integration test (see smoke.sh).
#
# Build the production builder and image first, from the repository root:
#   docker build -f packaging/docker/Dockerfile --target builder -t bookclerk-builder:abs-ci .
#   docker build -f packaging/docker/Dockerfile -t bookclerk:abs-ci .
# then this file (docker-compose.yml does it with the same context).
#
# The plugin is staged the way `cargo stage-plugins` lays it out and found
# through BOOKCLERK_PLUGIN_DIRS; the test approves, configures and deploys it
# with the CLI like an operator would. Everything else, isolation included,
# is the production image unchanged.

ARG BOOKCLERK_BUILDER=bookclerk-builder:abs-ci
ARG BOOKCLERK_IMAGE=bookclerk:abs-ci

FROM ${BOOKCLERK_BUILDER} AS plugin
RUN cargo stage-plugins --release --plugin audiobookshelf --dest /plugins \
    && test -f "$(find /plugins -mindepth 2 -maxdepth 2 -name plugin.toml | head -n1)"

FROM ${BOOKCLERK_IMAGE}
COPY --from=plugin /plugins /opt/bookclerk-test-plugins
ENV BOOKCLERK_PLUGIN_DIRS=/opt/bookclerk-test-plugins
