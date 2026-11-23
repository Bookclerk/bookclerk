#!/bin/sh
# Bakes the seeded library the hosted demo image ships, at image build time
# (the `seeded` stage of packaging/demo/Dockerfile), so a visitor's container
# copies it instead of seeding at boot. Runs as the bookclerk user, with
# network access, with BOOKCLERK_FILES_DIR pointing at an empty template dir.
#
#   1. bookclerk-demo-seed adds Sam, his books, wishlist and offline branch,
#      and approves and deploys the storefronts and covers.
#   2. bookclerkd runs until it has replayed the seed's events, downloaded
#      the default embedding model into the embedder's data dir (verified
#      against the digests pinned in the plugin package), embedded the
#      library with it and stored Sam's Discover shelves ranked on those
#      vectors, then stops. The vectors ship in the template and the model
#      beside it (step 5), so a container neither downloads the model nor
#      embeds the library.
#   3. With bookclerkd stopped, bookclerk-demo-seed --list-covers lists the
#      covers a new session's first pages show (the Library and the store
#      listings Discover ranks from); bookclerkd then runs again while
#      bookclerk-demo-seed --bake-covers requests each one, so the cover
#      cache ships with them and a container does not download them while
#      the visitor waits. The cache is keyed by image URL, not by the
#      signing key step 4 deletes, so the entries stay valid.
#   4. bookclerk-demo-seed --bake-finish strips this install's own state:
#      the library secret fingerprint, leases and heartbeats in the library
#      (refusing a library that holds any sealed secret or session), and
#      master.key, the covers URL key, plugin sessions and logs on disk.
#   5. The model's files move out of the template into
#      $BOOKCLERK_DEMO_MODELS_OUT, which the image ships as
#      /opt/bookclerk/models and boot.sh copies back into the embedder's
#      data dir. They are a layer of their own that way, so a seed refresh
#      (Dockerfile.refresh) neither downloads them again nor adds a second
#      copy to the image: it starts from the models the image holds.
#   6. The check below fails the build if any secret file is left.
#
# The node identity (node-identity.json: node_id and library_id) is baked
# too, so every demo container shares the same two ids. That is acceptable:
# each visitor's container is a VM and library of its own that never joins
# a library, the ids carry no secret (the library secret is the per-container
# master.key), and the baked plugin deployments are keyed by the node id, so
# a new id would orphan them. The ids also appear in the diagnostics reports
# the copies send, where every demo shares them.
set -eu

dir=${BOOKCLERK_FILES_DIR:?BOOKCLERK_FILES_DIR must name the template dir}

# The bake downloads the embedding model, which a host does by default; a
# container never does (see boot.sh), so the template must hold it. The
# config file is removed below so none ships: a container writes its own.
bake_config=${BOOKCLERK_CONFIG:-$dir/config.toml}
: >"$bake_config"

# A refresh: the image already holds the model, and its template names the
# embedder's models dir, so put the files there and nothing is downloaded.
# The host accepts them only when they are the pinned ones.
shipped_models=/opt/bookclerk/models
for old_models in /opt/bookclerk/seeded/plugin-state/*/data/models; do
	[ -d "$old_models" ] && [ -d "$shipped_models" ] || continue
	place=$dir${old_models#/opt/bookclerk/seeded}
	mkdir -p "$place"
	cp -a "$shipped_models/." "$place/" ||
		echo "bake-seed: could not reuse the image's model; downloading it" >&2
done

/opt/bookclerk/bookclerk-demo-seed

# A token for this build step only: it lives in the environment of
# bookclerkd and the CLI, never on argv or on disk, and the daemon does not
# seal an environment token. No auth password: the key it would wrap is
# deleted below.
BOOKCLERK_OPERATOR_TOKEN=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
export BOOKCLERK_OPERATOR_TOKEN

# Starts bookclerkd in the background and waits until it answers; fails
# when it does not answer within 120 s (the daemon may still be running).
start_daemon() {
	/opt/bookclerk/bookclerkd &
	daemon=$!
	trap 'kill "$daemon" 2>/dev/null || true' EXIT
	waited=0
	until bookclerk daemon status >/dev/null 2>&1; do
		waited=$((waited + 1))
		if [ "$waited" -gt 120 ]; then
			echo "bake-seed: bookclerkd did not answer within 120 s" >&2
			return 1
		fi
		sleep 1
	done
}

# Stops the bookclerkd start_daemon started.
stop_daemon() {
	kill -TERM "$daemon" 2>/dev/null || true
	wait "$daemon" || true
	trap - EXIT
}

start_daemon || exit 1

# Done when the library is embedded with the model (a discover_embed job
# succeeded and reports the model's vectors serve; the daemon queues it once
# the model is installed), a Discover ranking succeeded after that and no
# job is pending or running, three checks in a row. A ranking that had
# already succeeded before the embedding was seen ranked on the stand-in's
# vectors, so it is remembered by job id and does not count: the embedding
# queues the ranking again, and that later one stores the shelves ranked on
# the model's vectors. Past the cap the bake keeps what is stored only when
# such a ranking finished (a container recomputes the rest after it
# starts); otherwise the build fails below.
waited=0
quiet=0
embedded=0
ranked=0
stand_in_ranked=' '
while [ "$quiet" -lt 3 ]; do
	jobs=$(bookclerk daemon jobs 2>/dev/null || true)
	if printf '%s\n' "$jobs" |
		grep 'kind=discover_embed status=succeeded' | grep -q 'serving=true'; then
		embedded=1
	fi
	for id in $(printf '%s\n' "$jobs" |
		sed -n 's/^\([^ ]*\) kind=discover_feed status=succeeded.*/\1/p'); do
		case $stand_in_ranked in
		*" $id "*) ;;
		*)
			if [ "$embedded" -eq 1 ]; then
				ranked=1
			else
				stand_in_ranked="$stand_in_ranked$id "
			fi
			;;
		esac
	done
	if [ "$ranked" -eq 1 ] &&
		! printf '%s\n' "$jobs" | grep -Eq 'status=(pending|running)'; then
		quiet=$((quiet + 1))
	else
		quiet=0
	fi
	waited=$((waited + 1))
	if [ "$waited" -gt 420 ]; then
		echo "bake-seed: Discover still busy after 420 s" >&2
		break
	fi
	sleep 1
done

stop_daemon

# The demo's Discover and search need the model: without it every container
# would quietly fall back to the stand-in ranking, so the build fails and
# the deployed demo stays as it is.
model=$(find "$dir/plugin-state" -path '*/data/models/*/model.json' -print 2>/dev/null | head -n 1)
if [ -z "$model" ] || [ "$embedded" -ne 1 ]; then
	echo "bake-seed: the embedding model was not installed, or the library was not embedded with it" >&2
	exit 1
fi
if [ "$ranked" -ne 1 ]; then
	echo "bake-seed: no Discover ranking finished after the library was embedded" >&2
	exit 1
fi

# Step 5, first half: copy the model out. It stays in the template until
# the covers are baked, so the bookclerkd that serves them finds it there
# and does not download it again.
models_dir=${model%/*/model.json}
if [ -n "${BOOKCLERK_DEMO_MODELS_OUT:-}" ]; then
	cp -a "$models_dir/." "$BOOKCLERK_DEMO_MODELS_OUT/"
fi

# Bake the covers (step 3). Best effort: a cover that fails is fetched by
# the container that first shows it.
covers=$(mktemp)
if ! /opt/bookclerk/bookclerk-demo-seed --list-covers "$covers"; then
	echo "bake-seed: listing covers failed; containers fetch them" >&2
elif ! start_daemon; then
	echo "bake-seed: bookclerkd did not restart; containers fetch the covers" >&2
	stop_daemon
else
	listen=${BOOKCLERK_DAEMON_LISTEN:-127.0.0.1:8787}
	listen=${listen%%,*}
	/opt/bookclerk/bookclerk-demo-seed --bake-covers "$covers" "http://127.0.0.1:${listen##*:}" ||
		echo "bake-seed: baking covers failed; containers fetch them" >&2
	stop_daemon
fi
rm -f "$covers"
unset BOOKCLERK_OPERATOR_TOKEN

# Step 5, second half: no bookclerkd runs again, so empty the template's
# models dir. The empty dir stays: it tells boot.sh (and the next refresh)
# where the files go.
rm -rf "$models_dir"
mkdir -p "$models_dir"

/opt/bookclerk/bookclerk-demo-seed --bake-finish
rm -f "$bake_config"

# Nothing that belongs to one container may ship in the template.
leftover=$(find "$dir" \( -name master.key -o -name token.key -o -name 'session-*' \) -print)
if [ -n "$leftover" ]; then
	echo "bake-seed: refusing to bake, these must not ship:" >&2
	printf '%s\n' "$leftover" >&2
	exit 1
fi
test -f "$dir/library.db"
echo "bake-seed: baked $(du -sk "$dir" | cut -f1) KiB"
