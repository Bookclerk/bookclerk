#!/usr/bin/env bash
# Triage diagnostics reports on the collector's Workers AI and file GitHub issues.
#
# Anyone can submit a report, so the model (behind the collector's POST
# /triage) has no tools and only returns a JSON plan; this script validates
# the plan and files the issues itself with the workflow token.
#
# Usage: triage-and-file.sh reports.json
# Env: GITHUB_REPOSITORY, GH_TOKEN (issues: write), COLLECTOR_BASE_URL,
#      REPORT_API_KEY (collector key; sent only to COLLECTOR_BASE_URL).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

REPORTS_FILE=${1:?usage: triage-and-file.sh reports.json}
REPO=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY required}

BASE=${COLLECTOR_BASE_URL:?COLLECTOR_BASE_URL required}
: "${REPORT_API_KEY:?REPORT_API_KEY required}"
: "${GH_TOKEN:?GH_TOKEN required (issues: write)}"

count=$(jq '.count // (.reports | length) // 0' "$REPORTS_FILE")
if [[ "$count" -eq 0 ]]; then
  echo "No reports to analyze."
  exit 0
fi

# Build a sanitized, size-capped data blob. Strip C0 controls except \n\t.
# Create both temps before the EXIT trap so `set -u` cannot trip on an unset name.
DATA_FILE=$(mktemp)
PLAN_FILE=$(mktemp)
RESPONSE_FILE=$(mktemp)
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$DATA_FILE" "$PLAN_FILE" "$RESPONSE_FILE" "$WORK_DIR"' EXIT

# Cap by complete JSON array elements (never mid-object) so the model always gets valid JSON.
jq -c '
  .reports
  | map({
      report_id: (.report_id // .report.report_id // .file_name),
      file_name,
      upload_timestamp_ms,
      trigger: (.report.payload.trigger // .report.trigger // "report"),
      version: (.report.payload.version // .report.version // "?"),
      os: (.report.payload.os // .report.os // "?"),
      arch: (.report.payload.arch // null),
      distro: (.report.payload.distro // null),
      rustc_channel: (.report.payload.rustc_channel // null),
      received_at_unix_ms: (.report.received_at_unix_ms // null),
      event_count: ((.report.payload.events // .report.events // []) | length),
      events: ((.report.payload.events // .report.events // []) | .[0:40]
        | map({
            level,
            target,
            message: ((.message | strings) // "" | .[0:500]),
            fields: ((.fields | arrays) // [] | .[0:20])
          }))
    })
' "$REPORTS_FILE" \
  | tr -d '\000-\010\013\014\016-\037' \
  | python3 -c '
import json, sys
LIMIT = 120_000
raw = sys.stdin.read()
try:
    data = json.loads(raw)
except json.JSONDecodeError:
    sys.stdout.write("[]")
    raise SystemExit(0)
if not isinstance(data, list):
    data = [data]
kept = []
size = 2  # []
for item in data:
    piece = json.dumps(item, separators=(",", ":"), ensure_ascii=False)
    add = len(piece.encode("utf-8")) + (1 if kept else 0)
    if size + add > LIMIT:
        break
    kept.append(item)
    size += add
json.dump(kept, sys.stdout, separators=(",", ":"), ensure_ascii=False)
' > "$DATA_FILE"

# Report IDs as a Markdown list, one per line. IDs are collector object names
# or report UUIDs; anything else is listed as "unknown" so a report cannot
# write Markdown into an issue through its ID. A report UUID also links to its
# dashboard page on the collector (organization members only), in the same
# form validate_plan.py writes and dashboard/report-ids.js reads.
id_list() {
  jq -r --arg base "${BASE%/}" "$1"' | if type == "string" and test("^[A-Za-z0-9._:-]{1,128}$") then . else "unknown" end
    | if test("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
      then "- `\(.)` \u00b7 [dashboard](\($base)/reports/\(.))"
      else "- `\(.)`" end' "$2"
}
ALL_IDS=$(id_list '.reports[] | (.report_id // .report.report_id // .file_name)' "$REPORTS_FILE")
KEPT_IDS=$(id_list '.[] | .report_id' "$DATA_FILE")
# Reports the size cap left out of the batch: the plan cannot cover them.
OMITTED_IDS=$(comm -23 <(sort <<<"$ALL_IDS") <(sort <<<"$KEPT_IDS"))

# One issue tracking reports no plan covers, by ID only.
# Usage: file_fallback <report count> <reason> <ID list>.
file_fallback() {
  gh label create diagnostics --color "0E8A16" --description "Automated diagnostics reports" 2>/dev/null || true
  local url
  url=$({
    echo "<!-- bookclerk-diagnostics: triage fallback -->"
    echo "## Diagnostics batch ($1 report(s))"
    echo
    echo "### Report IDs"
    printf '%s\n' "$3"
    echo
    echo "Automated triage did not cover these reports ($2). Review them"
    echo "on the dashboard (organization members only); report contents are"
    echo "not posted here."
    echo
    echo "Workflow run: ${GITHUB_SERVER_URL:-https://github.com}/${REPO}/actions/runs/${GITHUB_RUN_ID:-unknown}"
  } | gh issue create --repo "$REPO" --title "[diagnostics] unattended batch ($1)" --label diagnostics --body-file -) || return 1
  echo "$url"
  notify_issue "$url"
}

# The prompt lives in the collector (triage.js), next to the model and its
# neuron budget; this only sends the batch. The key goes in a header read
# from stdin, never on argv.
set +e
http=$(printf 'header = "Authorization: Bearer %s"\n' "$REPORT_API_KEY" \
  | curl -sS --config - -o "$RESPONSE_FILE" -w "%{http_code}" \
      --max-time 120 \
      -H "Content-Type: application/json" \
      --data-binary "@$DATA_FILE" \
      "${BASE%/}/triage")
curl_status=$?
set -e
# Only curl needs the key; nothing after it (the plan validator reads
# report-steered model output) inherits it. A shell-only copy, never
# exported, lets notify_issue tell the collector about filed issues.
notify_key=$REPORT_API_KEY
unset REPORT_API_KEY

# Tells the collector's dashboard about an issue just filed, so it links the
# issue's reports at once instead of at the next sync. Best effort: the
# dashboard's 30-minute sync catches anything this misses.
# Usage: notify_issue <issue URL printed by gh issue create>.
notify_issue() {
  local number
  number=$(sed -n 's#^https://[^ ]*/issues/\([0-9][0-9]*\)$#\1#p' <<<"$1" | tail -n 1)
  [[ -n "$number" ]] || return 0
  printf 'header = "Authorization: Bearer %s"\n' "$notify_key" \
    | curl -fsS --config - -o /dev/null --max-time 30 \
        -H "Content-Type: application/json" \
        --data-binary "{\"issue\":${number}}" \
        "${BASE%/}/issues/notify" \
    || echo "::warning::Could not tell the dashboard about issue #${number}; the next sync will link it" >&2
}
status=1
reason="the collector could not be reached"
if [[ $curl_status -eq 0 ]]; then
  case "$http" in
    200)
      if jq -er '.plan_text | strings' "$RESPONSE_FILE" >"$PLAN_FILE"; then
        status=0
        jq -r '"Triage used \(.neurons) neurons on \(.model) (\(.spent_today)/\(.cap) today)"' "$RESPONSE_FILE" || true
      else
        reason="the collector returned no plan"
      fi
      ;;
    429) reason="today's triage budget is used up" ;;
    *) reason="the collector answered HTTP ${http}" ;;
  esac
fi

# Validate the plan and turn it into issue files (validate_plan.py has the
# rules and fails when a non-empty plan holds no valid issue).
ISSUES_DIR="$WORK_DIR/issues"
mkdir -p "$ISSUES_DIR"
planned=0
if [[ $status -eq 0 ]]; then
  if python3 -I "$SCRIPT_DIR/validate_plan.py" "$PLAN_FILE" "$DATA_FILE" "$ISSUES_DIR" "${BASE%/}"
  then
    planned=$(find "$ISSUES_DIR" -mindepth 1 -maxdepth 1 -type d | wc -l)
  else
    status=1
  fi
fi

if [[ $status -eq 0 ]]; then
  if [[ $planned -gt 0 ]]; then
    gh label create diagnostics --color "0E8A16" --description "Automated diagnostics reports" 2>/dev/null || true
    gh label create bug --color "d73a4a" --description "Something isn't working" 2>/dev/null || true
  fi
  # File every planned issue even if one fails, then fail the step.
  failed=0
  for dir in "$ISSUES_DIR"/*/; do
    [[ -d "$dir" ]] || continue
    if url=$(gh issue create --repo "$REPO" \
      --title "$(cat "$dir/title")" \
      --label "$(cat "$dir/labels")" \
      --body-file "$dir/body"); then
      echo "$url"
      notify_issue "$url"
    else
      failed=1
    fi
  done
  echo "Triage planned ${planned} issue(s) for ${count} report(s)."
  if [[ -n "$OMITTED_IDS" ]]; then
    omitted=$(wc -l <<<"$OMITTED_IDS")
    echo "${omitted} report(s) did not fit in the triage batch; tracking them in a fallback issue." >&2
    file_fallback "$omitted" "they did not fit in the batch's size limit" "$OMITTED_IDS" || failed=1
  fi
  # Reports the model saw but no planned issue lists would otherwise never be
  # linked to an issue.
  UNCOVERED_IDS=""
  if [[ -s "$ISSUES_DIR/uncovered.json" ]]; then
    UNCOVERED_IDS=$(id_list '.[]' "$ISSUES_DIR/uncovered.json")
  fi
  if [[ -n "$UNCOVERED_IDS" ]]; then
    uncovered=$(wc -l <<<"$UNCOVERED_IDS")
    echo "${uncovered} report(s) are in no planned issue; tracking them in a fallback issue." >&2
    file_fallback "$uncovered" "the triage plan left them out" "$UNCOVERED_IDS" || failed=1
  fi
  if [[ $failed -ne 0 ]]; then
    echo "::error::Some planned diagnostics issues could not be filed" >&2
    exit 1
  fi
else
  echo "No valid triage plan (${reason}); creating a single fallback issue via gh." >&2
  file_fallback "$count" "$reason" "$ALL_IDS"
fi

exit 0
