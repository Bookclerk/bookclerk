#!/usr/bin/env bash
# Creates, or updates in place, the two rulesets that protect release tags
# (`v*`). Run it once per repository, as a repository administrator, with
# your own GitHub CLI login (`gh auth login`):
#
#   bash scripts/setup-release-rulesets.sh OWNER/REPO
#
# - "Release tags: creators": only repository administrators may create a
#   `v*` tag. Publishing a release in the web UI creates its tag, so an
#   administrator still publishes releases as before; the automation App,
#   whose token can write contents, and anyone else with write access
#   cannot.
# - "Release tags: history": nobody, administrators included, may move or
#   delete a `v*` tag once it exists.
#
# Release-triggered workflows build whatever commit a tag names, from that
# commit's own copy of the workflow, so these rulesets, not a check inside a
# workflow, are what keeps a release on reviewed code. See
# docs/contributing/packaging.md#release-tags.
#
# Requires: gh (GitHub CLI) 2.x.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 OWNER/REPO" >&2
  exit 2
fi
repo=$1
case "$repo" in
  */*) ;;
  *) echo "OWNER/REPO expected, got: $repo" >&2; exit 2 ;;
esac

# The built-in repository role id of Admin, which rulesets accept as a
# bypass actor.
admin_role=5

# Creates the ruleset named $1 from the JSON on stdin, or replaces the one
# that already has that name.
upsert() {
  local name=$1 body id
  body=$(cat)
  id=$(gh api "repos/$repo/rulesets?includes_parents=false" --paginate \
    --jq ".[] | select(.name == \"$name\") | .id" | head -n 1)
  if [ -n "$id" ]; then
    gh api --method PUT "repos/$repo/rulesets/$id" --input - <<<"$body" >/dev/null
    echo "updated ruleset: $name"
  else
    gh api --method POST "repos/$repo/rulesets" --input - <<<"$body" >/dev/null
    echo "created ruleset: $name"
  fi
}

upsert "Release tags: creators" <<JSON
{
  "name": "Release tags: creators",
  "target": "tag",
  "enforcement": "active",
  "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}},
  "rules": [{"type": "creation"}],
  "bypass_actors": [{"actor_id": $admin_role, "actor_type": "RepositoryRole", "bypass_mode": "always"}]
}
JSON

upsert "Release tags: history" <<JSON
{
  "name": "Release tags: history",
  "target": "tag",
  "enforcement": "active",
  "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}},
  "rules": [{"type": "update"}, {"type": "deletion"}],
  "bypass_actors": []
}
JSON
