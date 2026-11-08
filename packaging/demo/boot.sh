#!/bin/sh
# Entrypoint of the hosted demo image.
#
# A fresh container copies the library baked at image build time
# (/opt/bookclerk/seeded, see bake-seed.sh) into its empty files dir, then
# hands the process to bookclerkd, which mints and wraps this container's own
# master.key, records its fingerprint and creates the covers URL key. When
# there is no baked library, or copying it fails, the container seeds live
# as before; a seed failure is logged and bookclerkd starts with an empty
# library rather than not at all.
set -u

dir=${BOOKCLERK_FILES_DIR:-/config}
template=/opt/bookclerk/seeded

if [ ! -e "$dir/library.db" ] && [ -f "$template/library.db" ]; then
	if cp -a "$template/." "$dir/"; then
		echo "demo: starting from the baked library" >&2
	else
		echo "demo: copying the baked library failed; seeding live" >&2
		# The files dir had no library before the copy, so everything in it
		# is the partial copy.
		find "$dir" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
	fi
fi

if [ ! -e "$dir/library.db" ]; then
	/opt/bookclerk/bookclerk-demo-seed ||
		echo 'bookclerk-demo-seed failed; starting with an empty library' >&2
fi

exec /opt/bookclerk/bookclerkd
