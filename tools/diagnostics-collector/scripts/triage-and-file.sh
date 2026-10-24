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

# Reports the collector marked as regressions of a closed issue (they run the
# version its fix shipped in, or a later one, and share its error signature)
# get their own issue per closed issue and version line, filed below without
# the model: regression_issue.py plans them and names the IDs it covers, which
# leave the triage batch here.
# The planner reads report-steered input, so the collector key is kept out
# of its environment like the other readers below.
REGRESS_DIR="$WORK_DIR/regressions"
env -u REPORT_API_KEY python3 -I "$SCRIPT_DIR/regression_issue.py" "$REPORTS_FILE" "$REGRESS_DIR" "${BASE%/}"
regressions=$(find "$REGRESS_DIR" -mindepth 1 -maxdepth 1 -type d | wc -l)
if [[ $regressions -gt 0 ]]; then
  TRIAGE_FILE="$WORK_DIR/triage-reports.json"
  jq --slurpfile handled "$REGRESS_DIR/handled.json" '
    ($handled[0].handled | map({key: ., value: true}) | from_entries) as $drop
    | .reports |= map(select(($drop[(.report_id // .report.report_id // .file_name) | tostring] // false) | not))
    | .count = (.reports | length)
  ' "$REPORTS_FILE" > "$TRIAGE_FILE"
  REPORTS_FILE=$TRIAGE_FILE
  echo "$(jq '.handled | length' "$REGRESS_DIR/handled.json") report(s) are regressions of closed issues; ${regressions} regression issue(s) to file."
  count=$(jq '.reports | length' "$REPORTS_FILE")
fi

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

# Title of the one open issue that holds reports no plan covers.
HOLDING_TITLE="[diagnostics] untriaged reports"
RUN_URL="${GITHUB_SERVER_URL:-https://github.com}/${REPO}/actions/runs/${GITHUB_RUN_ID:-unknown}"

# Adds reports no plan covers, by ID only, to the open holding issue: its
# Report IDs section grows (holding_issue.py edits only that list) and one
# comment per run says why. When there is no open holding issue, or it is
# full, a new one is opened. Usage: file_fallback <report count> <reason> <ID list>.
file_fallback() {
  gh label create diagnostics --color "0E8A16" --description "Automated diagnostics reports" 2>/dev/null || true
  local number body url
  # The newest open diagnostics issue with exactly this title (a full one
  # stays open behind its successor). The search narrows the listing; the
  # exact title match is done here.
  number=$(gh issue list --repo "$REPO" --state open --label diagnostics --limit 100 \
    --search "untriaged reports in:title" --json number,title \
    | jq -r --arg t "$HOLDING_TITLE" '[.[] | select(.title == $t) | .number] | max // empty') || number=""
  if [[ -n "$number" ]]; then
    gh issue view "$number" --repo "$REPO" --json body --jq .body > "$WORK_DIR/holding-body"
    printf '%s\n' "$3" > "$WORK_DIR/holding-lines"
    if body=$(python3 -I "$SCRIPT_DIR/holding_issue.py" "$WORK_DIR/holding-body" "$WORK_DIR/holding-lines"); then
      printf '%s' "$body" > "$WORK_DIR/holding-new"
      gh issue edit "$number" --repo "$REPO" --body-file "$WORK_DIR/holding-new" >/dev/null || return 1
      # The IDs are recorded once the edit succeeds; a failed comment only loses the reason.
      gh issue comment "$number" --repo "$REPO" --body "$1 more report(s) held: $2. Workflow run: $RUN_URL" >/dev/null \
        || echo "::warning::Could not comment on holding issue #${number}" >&2
      url="${GITHUB_SERVER_URL:-https://github.com}/${REPO}/issues/${number}"
      echo "$url"
      notify_issue "$url"
      return 0
    fi
    echo "::notice::Holding issue #${number} cannot take more reports; opening a new one" >&2
  fi
  url=$({
    echo "<!-- bookclerk-diagnostics: triage fallback -->"
    echo "## Untriaged reports"
    echo
    echo "### Report IDs"
    printf '%s\n' "$3"
    echo
    echo "Reports no triage plan covered, added by each ingest run that could not"
    echo "place them (the run's comment says why). Review them on the dashboard"
    echo "(organization members only); report contents are not posted here."
  } | gh issue create --repo "$REPO" --title "$HOLDING_TITLE" --label diagnostics --body-file -) || return 1
  echo "$url"
  gh issue comment "${url##*/}" --repo "$REPO" --body "$1 report(s) held: $2. Workflow run: $RUN_URL" >/dev/null || true
  notify_issue "$url"
}

# The prompt lives in the collector (triage.js), next to the model and its
# neuron budget; this only sends the batch. The key goes in a header read
# from stdin, never on argv. A batch the regression issues emptied is not
# sent: the empty plan below files nothing.
http=200
curl_status=0
if [[ "$count" -gt 0 ]]; then
  set +e
  http=$(printf 'header = "Authorization: Bearer %s"\n' "$REPORT_API_KEY" \
    | curl -sS --config - -o "$RESPONSE_FILE" -w "%{http_code}" \
        --max-time 120 \
        -H "Content-Type: application/json" \
        --data-binary "@$DATA_FILE" \
        "${BASE%/}/triage")
  curl_status=$?
  set -e
else
  printf '{"plan_text":"{\\"issues\\":[]}"}' > "$RESPONSE_FILE"
fi
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
  reason="the collector answered HTTP ${http}"
  case "$http" in
    200)
      if jq -er '.plan_text | strings' "$RESPONSE_FILE" >"$PLAN_FILE"; then
        status=0
        [[ "$count" -eq 0 ]] || jq -r '"Triage used \(.neurons) neurons on \(.model) (\(.spent_today)/\(.cap) today)"' "$RESPONSE_FILE" || true
      else
        reason="the collector returned no plan"
      fi
      ;;
    429) reason="today's triage budget is used up" ;;
  esac
fi

# File the regression issues first: they do not depend on the model's plan.
# A failure keeps the step red, so the pull cursor stays put and the run is
# retried.
regress_failed=0
if [[ $regressions -gt 0 ]]; then
  gh label create diagnostics --color "0E8A16" --description "Automated diagnostics reports" 2>/dev/null || true
  gh label create bug --color "d73a4a" --description "Something isn't working" 2>/dev/null || true
  gh label create regression --color "B60205" --description "A fixed problem came back in a later version" 2>/dev/null || true
  for dir in "$REGRESS_DIR"/*/; do
    [[ -d "$dir" ]] || continue
    if url=$(gh issue create --repo "$REPO" \
      --title "$(cat "$dir/title")" \
      --label "$(cat "$dir/labels")" \
      --body-file "$dir/body"); then
      echo "$url"
      notify_issue "$url"
    else
      regress_failed=1
    fi
  done
  # Regressions of more closed issues than one run files are held like
  # untriaged reports; a member files or attaches them from the dashboard.
  OVERFLOW_IDS=$(id_list '.overflow[]' "$REGRESS_DIR/handled.json")
  if [[ -n "$OVERFLOW_IDS" ]]; then
    overflow=$(wc -l <<<"$OVERFLOW_IDS")
    echo "${overflow} regression report(s) are beyond the run's regression issue cap; adding them to the holding issue." >&2
    file_fallback "$overflow" "they are regressions of more closed issues than one run files" "$OVERFLOW_IDS" || regress_failed=1
  fi
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
    reason="the model's reply held no valid plan"
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
    echo "${omitted} report(s) did not fit in the triage batch; adding them to the holding issue." >&2
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
    echo "${uncovered} report(s) are in no planned issue; adding them to the holding issue." >&2
    file_fallback "$uncovered" "the triage plan left them out" "$UNCOVERED_IDS" || failed=1
  fi
  if [[ $failed -ne 0 || $regress_failed -ne 0 ]]; then
    echo "::error::Some planned diagnostics issues could not be filed" >&2
    exit 1
  fi
else
  echo "No valid triage plan (${reason}); adding every report to the holding issue." >&2
  file_fallback "$count" "$reason" "$ALL_IDS"
  if [[ $regress_failed -ne 0 ]]; then
    echo "::error::Some regression issues could not be filed" >&2
    exit 1
  fi
fi

exit 0
