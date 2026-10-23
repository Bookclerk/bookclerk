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

# Validate the plan and turn it into issue files: at most 5 issues, fixed
# labels, only report IDs that are in this batch (an issue needs at least
# one), bounded sizes, and no @-mentions, cross-references or forged Report
# IDs sections.
ISSUES_DIR="$WORK_DIR/issues"
mkdir -p "$ISSUES_DIR"
planned=0
if [[ $status -eq 0 ]]; then
  if python3 -I - "$PLAN_FILE" "$DATA_FILE" "$ISSUES_DIR" <<'PY'
import json, pathlib, re, sys

plan_path, data_path, out_dir = sys.argv[1:4]
text = pathlib.Path(plan_path).read_text(encoding="utf-8", errors="replace")
known = {
    r["report_id"]
    for r in json.loads(pathlib.Path(data_path).read_text())
    if isinstance(r.get("report_id"), str)
}

# Take the first JSON object with an "issues" key, wherever it sits in the reply.
decoder, plan = json.JSONDecoder(), None
for i, ch in enumerate(text):
    if ch != "{":
        continue
    try:
        obj, _ = decoder.raw_decode(text, i)
    except json.JSONDecodeError:
        continue
    if isinstance(obj, dict) and "issues" in obj:
        plan = obj
        break
if plan is None or not isinstance(plan["issues"], list):
    sys.exit("reply has no JSON plan")

def defang(s: str) -> str:
    # Drop every link (the model may be steered by a report into adding
    # phishing or tracking links), and break @user / @org/team mentions and
    # #123 / owner/repo#123 / GH-123 references.
    s = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", s)
    s = re.sub(r"(?i)\b(?:https?|ftp)://\S+|\bwww\.\S+", "[link removed]", s)
    s = re.sub(r"(?i)<\s*(?:a|img)\b[^>]*>", "", s)
    s = re.sub(r"@(?=[A-Za-z0-9])", "@\u200b", s)
    s = re.sub(r"(?i)\bGH-(?=\d)", "GH-\u200b", s)
    return re.sub(r"#(?=\d)", "#\u200b", s)

def write(path: pathlib.Path, value: str) -> None:
    path.write_text(value, encoding="utf-8", errors="replace")

n = 0
for issue in plan["issues"][:5]:
    if not isinstance(issue, dict):
        continue
    title, body, ids = issue.get("title"), issue.get("body"), issue.get("report_ids")
    if not isinstance(title, str) or not isinstance(body, str) or not isinstance(ids, list):
        continue
    ids = list(dict.fromkeys(i for i in ids if isinstance(i, str) and i in known))
    if not ids:
        continue
    title = " ".join(title.split())
    if not title.startswith("[diagnostics] "):
        title = "[diagnostics] " + title
    # Only the script writes the Report IDs section.
    body = re.sub(r"(?im)^\s*#+\s*report ids\b.*$", "", body[:8000])
    labels = "diagnostics,bug" if issue.get("bug") is True else "diagnostics"
    out = pathlib.Path(out_dir) / str(n)
    out.mkdir()
    write(out / "title", defang(title[:120]))
    write(out / "labels", labels)
    write(
        out / "body",
        "<!-- bookclerk-diagnostics: workers-ai triage -->\n"
        + defang(body)
        + "\n\n### Report IDs\n"
        + "".join(f"- `{i}`\n" for i in ids),
    )
    n += 1

# An empty plan is a decision; a non-empty one with nothing valid is a bad
# reply, so let the fallback issue track the batch.
if plan["issues"] and n == 0:
    sys.exit("plan has no valid issues")
PY
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
    gh issue create --repo "$REPO" \
      --title "$(cat "$dir/title")" \
      --label "$(cat "$dir/labels")" \
      --body-file "$dir/body" || failed=1
  done
  echo "Triage planned ${planned} issue(s) for ${count} report(s)."
  if [[ $failed -ne 0 ]]; then
    echo "::error::Some planned diagnostics issues could not be filed" >&2
    exit 1
  fi
else
  echo "No valid triage plan (${reason}); creating a single fallback issue via gh." >&2
  gh label create diagnostics --color "0E8A16" --description "Automated diagnostics reports" 2>/dev/null || true
  {
    echo "<!-- bookclerk-diagnostics: triage fallback -->"
    echo "## Diagnostics batch (${count} report(s))"
    echo
    echo "### Report IDs"
    jq -r '.[] | "- `\(.report_id // "unknown")`"' "$DATA_FILE" 2>/dev/null || true
    echo
    echo "Automated triage did not run (${reason}). Review the reports"
    echo "by ID in the collector's storage; report contents are not posted here."
    echo
    echo "Workflow run: ${GITHUB_SERVER_URL:-https://github.com}/${REPO}/actions/runs/${GITHUB_RUN_ID:-unknown}"
  } | gh issue create --repo "$REPO" --title "[diagnostics] unattended batch (${count})" --label diagnostics --body-file -
fi

exit 0
