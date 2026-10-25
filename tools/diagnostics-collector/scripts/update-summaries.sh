#!/usr/bin/env bash
# Keep one summary comment per diagnostics issue up to date.
#
# The collector's GET /issues/summaries names every issue whose figures
# changed since the comment was last written (reports tracked, installations,
# how they were linked, versions, platforms, newest reports). This script
# renders each (summary_comment.py, sans-IO and fuzzed), edits the existing
# comment marked `<!-- bookclerk-diagnostics: summary -->` in place (editing
# notifies nobody) or writes it the first time, and tells the collector which
# fingerprint it wrote (POST /issues/summary) so the issue is not handed out
# again until its figures change. The Worker stays read-only on GitHub; only
# this workflow's token writes.
#
# Usage: update-summaries.sh
# Env: GITHUB_REPOSITORY, GH_TOKEN (issues: write), COLLECTOR_BASE_URL,
#      REPORT_API_KEY (collector key; sent only to COLLECTOR_BASE_URL).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

REPO=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY required}
BASE=${COLLECTOR_BASE_URL:?COLLECTOR_BASE_URL required}
: "${REPORT_API_KEY:?REPORT_API_KEY required}"
: "${GH_TOKEN:?GH_TOKEN required (issues: write)}"

MARKER="<!-- bookclerk-diagnostics: summary -->"
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

# The key goes in a header read from stdin, never on argv.
set +e
http=$(printf 'header = "Authorization: Bearer %s"\n' "$REPORT_API_KEY" \
  | curl -sS --config - -o "$WORK_DIR/summaries.json" -w "%{http_code}" \
      --max-time 60 "${BASE%/}/issues/summaries")
curl_status=$?
set -e
if [[ $curl_status -ne 0 ]]; then
  echo "::warning::Could not reach the collector for issue summaries" >&2
  exit 1
fi
case "$http" in
  200) ;;
  204)
    echo "The collector has no dashboard database; no summary comments to keep."
    exit 0
    ;;
  *)
    echo "::warning::The collector answered HTTP ${http} for issue summaries" >&2
    exit 1
    ;;
esac

# The renderer reads report-steered input, so the collector key stays out of
# its environment; a shell-only copy, never exported, is used afterwards.
notify_key=$REPORT_API_KEY
unset REPORT_API_KEY
OUT_DIR="$WORK_DIR/comments"
python3 -I "$SCRIPT_DIR/summary_comment.py" "$WORK_DIR/summaries.json" "$OUT_DIR" "${BASE%/}"

# Records the fingerprint written on an issue. Usage: summary_done <issue> <fingerprint file>.
summary_done() {
  printf 'header = "Authorization: Bearer %s"\n' "$notify_key" \
    | curl -fsS --config - -o /dev/null --max-time 30 \
        -H "Content-Type: application/json" \
        --data-binary "{\"issue\":$1,\"fingerprint\":\"$(cat "$2")\"}" \
        "${BASE%/}/issues/summary"
}

# A write GitHub refuses for good (a locked issue answers 403 saying so, a
# deleted or transferred one 404 or 410) is acknowledged to the collector so
# the issue stops taking a summary slot every run; any other failure (a rate
# limit, which GitHub answers with 403 or 429, an outage) is retried next run
# and fails this step. Usage: settle <issue> <verb>; returns 0 when the
# failure was acknowledged.
settle() {
  if grep -qE "HTTP (404|410)\b" "$WORK_DIR/err" || { grep -qE "HTTP 403\b" "$WORK_DIR/err" && grep -qi "locked" "$WORK_DIR/err"; }; then
    echo "::warning::GitHub refuses to ${2} the summary comment on issue #${1} ($(head -c 200 "$WORK_DIR/err")); not retrying until its figures change" >&2
    return 0
  fi
  echo "::warning::Could not ${2} the summary comment on issue #${1}: $(head -c 300 "$WORK_DIR/err")" >&2
  failed=1
  return 1
}

updated=0
created=0
failed=0
for dir in "$OUT_DIR"/*/; do
  [[ -d "$dir" ]] || continue
  number=$(cat "$dir/issue")
  # The summary comment is the first one a bot account (this workflow's
  # token writes as one) posted that starts with the marker. A failed listing
  # skips the issue rather than writing a second comment.
  if ! ids=$(gh api --paginate "repos/${REPO}/issues/${number}/comments?per_page=100" \
    --jq ".[] | select(.user.type == \"Bot\" and (.body | startswith(\"${MARKER}\"))) | .id" 2>"$WORK_DIR/err"); then
    echo "::warning::Could not list the comments of issue #${number}: $(head -c 300 "$WORK_DIR/err")" >&2
    failed=1
    continue
  fi
  comment_id=$(head -n 1 <<<"$ids")
  if [[ -n "$comment_id" ]]; then
    if gh api -X PATCH "repos/${REPO}/issues/comments/${comment_id}" -F "body=@${dir}/body" >/dev/null 2>"$WORK_DIR/err"; then
      updated=$((updated + 1))
    else
      settle "$number" "edit" || continue
    fi
  elif gh api -X POST "repos/${REPO}/issues/${number}/comments" -F "body=@${dir}/body" >/dev/null 2>"$WORK_DIR/err"; then
    created=$((created + 1))
  else
    settle "$number" "write" || continue
  fi
  summary_done "$number" "$dir/fingerprint" \
    || echo "::warning::Could not record the summary of issue #${number}; it will be written again next run" >&2
done
echo "Summary comments: ${updated} updated, ${created} created."
exit $failed
