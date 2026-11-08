#!/bin/sh
# Entrypoint of the hosted demo image.
#
# A fresh container copies the library baked at image build time
# (/opt/bookclerk/seeded, see bake-seed.sh) into its files dir, then hands
# the process to bookclerkd, which mints and wraps this container's own
# master.key, records its fingerprint and creates the covers URL key. When
# there is no baked library, or copying it fails, the container seeds live
# as before; a seed failure is logged and bookclerkd starts with an empty
# library rather than not at all, and the next boot tries again.
set -u

dir=${BOOKCLERK_FILES_DIR:-/config}
template=/opt/bookclerk/seeded
# Written once the library holds the sample data, by either path.
marker="$dir/.demo-seeded"

# Copies the template into a staging dir first, so a failed copy leaves the
# files dir as it was, then moves each entry in, never over one already
# there (a mounted config.toml wins). If a move fails, the entries already
# moved in go back, so the files dir is again as it was.
use_template() {
	staging=$(mktemp -d "$dir/.baked.XXXXXX") || return 1
	moved=""
	ok=0
	if cp -a "$template/." "$staging/"; then
		for entry in "$staging"/* "$staging"/.[!.]*; do
			[ -e "$entry" ] || continue
			name=${entry##*/}
			[ -e "$dir/$name" ] && continue
			if mv "$entry" "$dir/"; then
				moved="$moved $name"
			else
				ok=1
				break
			fi
		done
	else
		ok=1
	fi
	if [ "$ok" -ne 0 ]; then
		# Template entry names have no spaces (see bake-seed.sh).
		for name in $moved; do
			rm -rf "${dir:?}/$name"
		done
	fi
	rm -rf "$staging"
	return "$ok"
}

if [ ! -e "$marker" ] && [ ! -e "$dir/library.db" ] && [ -f "$template/library.db" ]; then
	if use_template; then
		touch "$marker"
		echo "demo: starting from the baked library" >&2
	else
		echo "demo: copying the baked library failed; seeding live" >&2
	fi
fi

if [ ! -e "$marker" ]; then
	if /opt/bookclerk/bookclerk-demo-seed; then
		touch "$marker"
	else
		echo 'bookclerk-demo-seed failed; starting with an empty library' >&2
	fi
fi

exec /opt/bookclerk/bookclerkd
