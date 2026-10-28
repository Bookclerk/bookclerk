#!/usr/bin/env bash
# Creates, or updates in place, the two rulesets that protect the CLA
# signature branch. Run it once per repository, as a repository
# administrator, with your own GitHub CLI login (`gh auth login`):
#
#   bash setup-rulesets.sh OWNER/REPO APP_SLUG
#
# APP_SLUG is the GitHub App whose token records signatures (the part after
# github.com/apps/ in the App's public page URL). The App gets no new
# permission: only the first ruleset lists it, so it may add commits, and
# the second binds everyone, the App included.
#
# Requires: gh (GitHub CLI) 2.x.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 OWNER/REPO APP_SLUG" >&2
  exit 2
fi
repo=$1
slug=$2
branch=cla-signatures
case "$repo" in
  */*) ;;
  *) echo "OWNER/REPO expected, got: $repo" >&2; exit 2 ;;
esac

app_id=$(gh api "apps/$slug" --jq .id)
case "$app_id" in
  '' | *[!0-9]*) echo "could not find the GitHub App $slug" >&2; exit 1 ;;
esac

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

# Only the App may create or add commits to the branch.
upsert "CLA signatures: writers" <<JSON
{
  "name": "CLA signatures: writers",
  "target": "branch",
  "enforcement": "active",
  "conditions": {"ref_name": {"include": ["refs/heads/$branch"], "exclude": []}},
  "rules": [{"type": "creation"}, {"type": "update"}],
  "bypass_actors": [{"actor_id": $app_id, "actor_type": "Integration", "bypass_mode": "always"}]
}
JSON

# Nobody, the App included, may delete or rewrite the branch, merge into it
# or push unsigned commits.
upsert "CLA signatures: history" <<JSON
{
  "name": "CLA signatures: history",
  "target": "branch",
  "enforcement": "active",
  "conditions": {"ref_name": {"include": ["refs/heads/$branch"], "exclude": []}},
  "rules": [
    {"type": "deletion"},
    {"type": "non_fast_forward"},
    {"type": "required_linear_history"},
    {"type": "required_signatures"}
  ],
  "bypass_actors": []
}
JSON
