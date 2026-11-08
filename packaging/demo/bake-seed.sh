#!/bin/sh
# Bakes the seeded library the hosted demo image ships, at image build time
# (the `seeded` stage of packaging/demo/Dockerfile), so a visitor's container
# copies it instead of seeding at boot. Runs as the bookclerk user, with
# network access, with BOOKCLERK_FILES_DIR pointing at an empty template dir.
#
#   1. bookclerk-demo-seed adds Sam, his books, wishlist and offline branch,
#      and approves and deploys the storefronts and covers.
#   2. bookclerkd runs until it has replayed the seed's events and stored
#      Sam's Discover shelves, then stops.
#   3. bookclerk-demo-seed --bake-finish strips this install's own state:
#      the cluster secret fingerprint, leases and heartbeats in the library
#      (refusing a library that holds any sealed secret or session), and
#      master.key, the covers URL key, plugin sessions and logs on disk.
#   4. The check below fails the build if any secret file is left.
#
# The node identity (node-identity.json: node_id and cluster_id) is baked
# too, so every demo container shares the same two ids. That is acceptable:
# each visitor's container is a VM and library of its own that never joins
# a cluster, the ids carry no secret (the cluster secret is the per-container
# master.key), and the baked plugin deployments are keyed by the node id, so
# a new id would orphan them. The ids only appear in diagnostics reports,
# which the demo does not send.
set -eu

dir=${BOOKCLERK_FILES_DIR:?BOOKCLERK_FILES_DIR must name the template dir}

/opt/bookclerk/bookclerk-demo-seed

# A token for this build step only: it lives in the environment of
# bookclerkd and the CLI, never on argv or on disk, and the daemon does not
# seal an environment token. No auth password: the key it would wrap is
# deleted below.
BOOKCLERK_OPERATOR_TOKEN=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
export BOOKCLERK_OPERATOR_TOKEN

/opt/bookclerk/bookclerkd &
daemon=$!
trap 'kill "$daemon" 2>/dev/null || true' EXIT

waited=0
until bookclerk daemon status >/dev/null 2>&1; do
	waited=$((waited + 1))
	if [ "$waited" -gt 120 ]; then
		echo "bake-seed: bookclerkd did not answer within 120 s" >&2
		exit 1
	fi
	sleep 1
done

# Done when a Discover feed is stored and no job is pending or running, three
# checks in a row. Past the cap the bake keeps whatever is stored, and a
# container recomputes the rest after it starts.
waited=0
quiet=0
while [ "$quiet" -lt 3 ]; do
	jobs=$(bookclerk daemon jobs 2>/dev/null || true)
	if printf '%s\n' "$jobs" | grep -q 'kind=discover_feed status=succeeded' &&
		! printf '%s\n' "$jobs" | grep -Eq 'status=(pending|running)'; then
		quiet=$((quiet + 1))
	else
		quiet=0
	fi
	waited=$((waited + 1))
	if [ "$waited" -gt 180 ]; then
		echo "bake-seed: Discover still busy after 180 s; baking what is stored" >&2
		break
	fi
	sleep 1
done

kill -TERM "$daemon"
wait "$daemon" || true
trap - EXIT
unset BOOKCLERK_OPERATOR_TOKEN

/opt/bookclerk/bookclerk-demo-seed --bake-finish

# Nothing that belongs to one container may ship in the template.
leftover=$(find "$dir" \( -name master.key -o -name token.key -o -name 'session-*' \) -print)
if [ -n "$leftover" ]; then
	echo "bake-seed: refusing to bake, these must not ship:" >&2
	printf '%s\n' "$leftover" >&2
	exit 1
fi
test -f "$dir/library.db"
echo "bake-seed: baked $(du -sk "$dir" | cut -f1) KiB"
