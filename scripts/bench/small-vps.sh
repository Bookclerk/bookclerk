#!/usr/bin/env bash
# 1 vCPU / 1 GiB envelope around bookclerkd and one background command.
#
# Creates a cgroup v2 child leaf and writes ceilings there. Never writes
# memory.max or cpu.max on the current or parent cgroup. If the leaf cannot
# be created or a controller file cannot be written, the script exits non-zero
# and does not publish host-wide numbers as the 1 GiB envelope.
#
#   cargo build --release -p bookclerkd -p bookclerk-cli
#   export BOOKCLERK_FILES_DIR="$PWD/BookclerkFiles/envelope"
#   cargo test -p bookclerk-library --test envelope_seed -- --ignored --nocapture
#   scripts/bench/small-vps.sh
#
# Metrics land next to the files directory as envelope-metrics.json.
# Numeric CI budgets are intentionally absent until that file exists.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FILES="${BOOKCLERK_FILES_DIR:-$ROOT/BookclerkFiles/envelope}"
LABEL="${ENVELOPE_LABEL:-measured}"
DAEMON="${BOOKCLERK_DAEMON_BIN:-$ROOT/target/release/bookclerkd}"
CLI="${BOOKCLERK_CLI_BIN:-$ROOT/target/release/bookclerk}"
METRICS="$(dirname "$FILES")/envelope-metrics.json"
DAEMON_PID=""
CGROUP=""
PARENT_CGROUP=""

fail() {
  echo "envelope: $*" >&2
  exit 1
}

cleanup() {
  local status=$?
  if [[ -n "${DAEMON_PID}" ]]; then
    kill "${DAEMON_PID}" 2>/dev/null || true
    wait "${DAEMON_PID}" 2>/dev/null || true
  fi
  if [[ -n "${PARENT_CGROUP}" && -d "${PARENT_CGROUP}" ]]; then
    echo $$ >"${PARENT_CGROUP}/cgroup.procs" 2>/dev/null || true
  fi
  if [[ -n "${CGROUP}" && -d "${CGROUP}" ]]; then
    rmdir "${CGROUP}" 2>/dev/null || true
  fi
  exit "${status}"
}
trap cleanup EXIT

[[ -x "${DAEMON}" ]] || fail "missing ${DAEMON}; cargo build --release -p bookclerkd -p bookclerk-cli"
[[ -x "${CLI}" ]] || fail "missing ${CLI}; cargo build --release -p bookclerkd -p bookclerk-cli"
[[ -f "${FILES}/library.db" ]] || fail "missing ${FILES}/library.db; run the envelope_seed test first"
[[ -f /proc/self/cgroup ]] || fail "no /proc/self/cgroup; refusing host-wide numbers"

current_rel=""
while IFS= read -r line; do
  case "${line}" in
    0::*) current_rel="${line#0::}" ;;
  esac
done </proc/self/cgroup
[[ -n "${current_rel}" ]] || fail "no cgroup v2 entry in /proc/self/cgroup"

if [[ "${current_rel}" == "/" || -z "${current_rel}" ]]; then
  PARENT_CGROUP="/sys/fs/cgroup"
else
  PARENT_CGROUP="/sys/fs/cgroup${current_rel}"
fi
[[ -d "${PARENT_CGROUP}" ]] || fail "current cgroup ${PARENT_CGROUP} is not a directory"

# Best-effort delegation. A populated parent often refuses this; the child
# create and controller writes below still fail closed.
if [[ -f "${PARENT_CGROUP}/cgroup.controllers" && -f "${PARENT_CGROUP}/cgroup.subtree_control" ]]; then
  available="$(<"${PARENT_CGROUP}/cgroup.controllers")"
  for name in memory cpu; do
    if [[ " ${available} " == *" ${name} "* ]]; then
      echo "+${name}" >"${PARENT_CGROUP}/cgroup.subtree_control" 2>/dev/null || true
    fi
  done
fi

CGROUP="${PARENT_CGROUP}/bookclerk-envelope"
if [[ -d "${CGROUP}" ]]; then
  fail "cgroup leaf ${CGROUP} already exists"
fi
if ! mkdir "${CGROUP}" 2>"${FILES}/cgroup-mkdir.err"; then
  fail "could not create child cgroup ${CGROUP} ($(cat "${FILES}/cgroup-mkdir.err")). Refusing to write limits on ${PARENT_CGROUP}."
fi

write_controller() {
  local name="$1"
  local value="$2"
  local path="${CGROUP}/${name}"
  if ! printf '%s\n' "${value}" >"${path}" 2>"${FILES}/cgroup-write.err"; then
    fail "could not write ${path} ($(cat "${FILES}/cgroup-write.err")). Refusing host-wide numbers as the 1 GiB envelope."
  fi
}

write_controller memory.max 1073741824
write_controller memory.swap.max 0
write_controller cpu.max "100000 100000"

echo $$ >"${CGROUP}/cgroup.procs" || fail "could not move the harness shell into ${CGROUP}"

cpu_max="$(tr -d '\n' <"${CGROUP}/cpu.max")"
memory_max="$(tr -d '\n' <"${CGROUP}/memory.max")"
[[ "${memory_max}" == "1073741824" ]] || fail "memory.max is ${memory_max}, not 1073741824"
[[ "${cpu_max}" == "100000 100000" ]] || fail "cpu.max is '${cpu_max}', not '100000 100000'"

affinity="$(python3 -c 'import os; print(len(os.sched_getaffinity(0)))')"
cpu_count="$(python3 -c 'import os; print(os.cpu_count() or 0)')"

du_bytes() {
  if [[ -e "$1" ]]; then
    du -sb "$1" | awk '{print $1}'
  else
    echo 0
  fi
}

object_root="${FILES}/objects"
cold_db="$(du_bytes "${FILES}/library.db")"
cold_wal="$(du_bytes "${FILES}/library.db-wal")"
cold_objects="$(du_bytes "${object_root}")"
cold_object_count="$(find "${object_root}" -type f 2>/dev/null | wc -l | tr -d ' ')"

scratch_snapshot() {
  python3 - "$FILES" <<'PY'
import json, os, sys
root = sys.argv[1]
names = [
    "cache",
    "search_index",
    os.path.join("objects", ".bookclerk-list-index"),
    os.path.join("objects", ".bookclerk-stage"),
    os.path.join("objects", "stage-journal"),
]
def du(path):
    total = 0
    if not os.path.exists(path):
        return 0
    for dirpath, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(dirpath, name))
            except OSError:
                pass
    return total
print(json.dumps({name: du(os.path.join(root, name)) for name in names}))
PY
}

reset_peak() {
  if printf '0\n' >"${CGROUP}/memory.peak" 2>/dev/null; then
    echo 1
  else
    echo 0
  fi
}

sample_proc() {
  local pid="$1"
  local rss hwm
  rss="$(awk '/^VmRSS:/ {print $2}' "/proc/${pid}/status")"
  hwm="$(awk '/^VmHWM:/ {print $2}' "/proc/${pid}/status")"
  python3 - "$CGROUP" "${rss:-0}" "${hwm:-0}" <<'PY'
import json, sys
cgroup, rss_kb, hwm_kb = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
def read(name):
    try:
        return open(f"{cgroup}/{name}", encoding="utf-8").read()
    except OSError:
        return ""
stat = {}
for line in read("memory.stat").splitlines():
    parts = line.split()
    if len(parts) == 2 and parts[1].isdigit():
        stat[parts[0]] = int(parts[1])
def num(text):
    token = text.split()
    if not token or not token[0].isdigit():
        return None
    return int(token[0])
print(json.dumps({
    "vm_rss_bytes": rss_kb * 1024,
    "vm_hwm_bytes": hwm_kb * 1024,
    "memory_current": num(read("memory.current")),
    "memory_peak": num(read("memory.peak")),
    "anon": stat.get("anon"),
    "file": stat.get("file"),
}))
PY
}

export BOOKCLERK_FILES_DIR="${FILES}"
log="${FILES}/bookclerkd-envelope.log"
: >"${log}"
start_ns="$(date +%s%N)"
"${DAEMON}" >"${log}" 2>&1 &
DAEMON_PID=$!
echo "${DAEMON_PID}" >"${CGROUP}/cgroup.procs" || fail "could not move bookclerkd ${DAEMON_PID} into ${CGROUP}"

health_ok=0
for _ in $(seq 1 120); do
  if curl -sf -o /dev/null "http://127.0.0.1:8787/health"; then
    health_ok=1
    break
  fi
  if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
    fail "bookclerkd exited before /health; see ${log}"
  fi
  sleep 0.5
done
[[ "${health_ok}" == 1 ]] || fail "GET /health did not return 200 within 60s; see ${log}"
end_ns="$(date +%s%N)"
startup_ms="$(( (end_ns - start_ns) / 1000000 ))"
sleep 10
idle_json="$(sample_proc "${DAEMON_PID}")"
media_line="$(grep -m1 'media pool:' "${log}" || true)"
token="$("${CLI}" daemon token | head -n 1)"
[[ -n "${token}" ]] || fail "bookclerk daemon token returned an empty token"

host_row="$(python3 - "${FILES}/library.db" <<'PY'
import json, sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.row_factory = sqlite3.Row
row = con.execute(
    "SELECT logical_cpus, cpu_max_quota_us, cpu_max_period_us, memory_max_bytes, "
    "memory_current_bytes, memory_anon_bytes, files_dir_free_bytes, scratch_bytes "
    "FROM hosts ORDER BY heartbeat_at DESC LIMIT 1"
).fetchone()
print(json.dumps(dict(row) if row else {}))
PY
)"

run_phase() {
  local phase="$1"
  local out="$2"
  ENVELOPE_PHASE="${phase}" ENVELOPE_PHASE_OUT="${out}" python3 - "${FILES}" "${token}" "${METRICS}" "${LABEL}" <<'PY'
import json, os, random, sys, threading, time, urllib.error, urllib.request
files, token, _metrics, _label = sys.argv[1:5]
base = "http://127.0.0.1:8787"
auth = {"Authorization": f"Bearer {token}"}
phase = os.environ["ENVELOPE_PHASE"]
out = os.environ["ENVELOPE_PHASE_OUT"]

def fetch(path, authed):
    headers = auth if authed else {}
    req = urllib.request.Request(base + path, headers=headers, method="GET")
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            resp.read()
            code = resp.status
    except urllib.error.HTTPError as err:
        code = err.code
        err.read()
    return code, (time.perf_counter() - started) * 1000

def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, int(round((p / 100) * (len(ordered) - 1)))))
    return ordered[index]

ROUTES = [
    ("GET /health", "/health", False),
    ("GET /api/status", "/api/status", True),
    ("GET /api/jobs", "/api/jobs", True),
    ("GET /api/library/books?limit=40&offset=0", "/api/library/books?limit=40&offset=0", True),
    ("GET /api/library/books?limit=40&offset=8000&status=acquired", "/api/library/books?limit=40&offset=8000&status=acquired", True),
    ("GET /api/library/books?account=envelope-b&limit=40", "/api/library/books?account=envelope-b&limit=40", True),
    ("GET /api/library/books?q=Title&limit=40", "/api/library/books?q=Title&limit=40", True),
    ("GET /api/library/books?q=Title&limit=8", "/api/library/books?q=Title&limit=8", True),
]

def time_route(path, authed, samples=40, clients=4):
    latencies, errors = [], []
    lock = threading.Lock()
    def worker(n):
        local, local_err = [], []
        for _ in range(n):
            code, ms = fetch(path, authed)
            local.append(ms)
            if code != 200:
                local_err.append(code)
        with lock:
            latencies.extend(local)
            errors.extend(local_err)
    each = max(1, samples // clients)
    threads = [threading.Thread(target=worker, args=(each,)) for _ in range(clients)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return {"samples": len(latencies), "errors": len(errors), "p50_ms": percentile(latencies, 50), "p95_ms": percentile(latencies, 95)}

def mix_for(seconds):
    pages = [
        "/api/library/books?limit=40&offset=0",
        "/api/library/books?limit=40&offset=40",
        "/api/library/books?limit=40&offset=400",
        "/api/library/books?limit=40&offset=4000",
        "/api/library/books?limit=40&offset=8000&status=acquired",
    ]
    buckets, errors = {}, 0
    lock = threading.Lock()
    stop = time.perf_counter() + seconds
    def worker():
        nonlocal errors
        while time.perf_counter() < stop:
            roll = random.random()
            if roll < 0.70:
                path, name = random.choice(pages), "books_page"
            elif roll < 0.85:
                path, name = "/api/status", "GET /api/status"
            elif roll < 0.95:
                path, name = "/api/jobs", "GET /api/jobs"
            else:
                path, name = "/api/library/books?q=Title&limit=40", "GET /api/library/books?q=Title"
            code, ms = fetch(path, True)
            with lock:
                buckets.setdefault(name, []).append(ms)
                if code != 200:
                    errors += 1
    threads = [threading.Thread(target=worker) for _ in range(4)]
    started = time.perf_counter()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    summary, total = {}, 0
    for name, values in buckets.items():
        total += len(values)
        summary[name] = {"samples": len(values), "p50_ms": percentile(values, 50), "p95_ms": percentile(values, 95)}
    return {"elapsed_s": time.perf_counter() - started, "requests": total, "errors": errors, "routes": summary}

if phase == "routes":
    report = {name: time_route(path, authed) for name, path, authed in ROUTES}
else:
    report = mix_for(60)
with open(out, "w", encoding="utf-8") as fh:
    json.dump(report, fh)
PY
}

scratch_before="$(scratch_snapshot)"
peak_reset_api="$(reset_peak)"
run_phase routes "${FILES}/envelope-routes.json"
run_phase mix "${FILES}/envelope-mix.json"
api_sample="$(sample_proc "${DAEMON_PID}")"
db_after_api="$(du_bytes "${FILES}/library.db")"
wal_after_api="$(du_bytes "${FILES}/library.db-wal")"

peak_reset_rebuild="$(reset_peak)"
scratch_before_rebuild="$(scratch_snapshot)"
rebuild_log="${FILES}/rebuild-envelope.log"
rebuild_start_ns="$(date +%s%N)"
"${CLI}" library search --rebuild-index --limit 0 >"${rebuild_log}" 2>&1 &
REBUILD_PID=$!
echo "${REBUILD_PID}" >"${CGROUP}/cgroup.procs" || fail "could not move rebuild ${REBUILD_PID} into ${CGROUP}"
run_phase mix "${FILES}/envelope-mix-rebuild.json" &
MIX_PID=$!
wait "${REBUILD_PID}" || fail "search rebuild failed; see ${rebuild_log}"
rebuild_end_ns="$(date +%s%N)"
wait "${MIX_PID}" || fail "rebuild-window load failed"
rebuild_ms="$(( (rebuild_end_ns - rebuild_start_ns) / 1000000 ))"
indexed="$(sed -n 's/search index rebuilt: \([0-9]*\) book(s)/\1/p' "${rebuild_log}" | head -n 1)"
rebuild_sample="$(sample_proc "${DAEMON_PID}")"
scratch_after="$(scratch_snapshot)"

python3 - \
  "${METRICS}" "${LABEL}" "${affinity}" "${cpu_count}" "${cpu_max}" "${memory_max}" \
  "${startup_ms}" "${idle_json}" "${media_line}" "${host_row}" \
  "${cold_db}" "${cold_wal}" "${cold_objects}" "${cold_object_count}" \
  "${scratch_before}" "${FILES}/envelope-routes.json" "${FILES}/envelope-mix.json" \
  "${api_sample}" "${peak_reset_api}" "${db_after_api}" "${wal_after_api}" \
  "${scratch_before_rebuild}" "${FILES}/envelope-mix-rebuild.json" "${rebuild_sample}" \
  "${peak_reset_rebuild}" "${rebuild_ms}" "${indexed:-0}" "${scratch_after}" <<'PY'
import json, sys
(path, label, affinity, cpu_count, cpu_max, memory_max, startup_ms, idle, media,
 host_row, cold_db, cold_wal, cold_objects, cold_object_count, scratch_before,
 routes, mix, api_sample, peak_reset_api, db_after_api, wal_after_api,
 scratch_before_rebuild, mix_rebuild, rebuild_sample, peak_reset_rebuild,
 rebuild_ms, indexed, scratch_after) = sys.argv[1:]
def load(p):
    with open(p, encoding="utf-8") as fh:
        return json.load(fh)
indexed_n = int(indexed or 0)
elapsed = int(rebuild_ms or 0)
doc = {
    "label": label,
    "budgets": None,
    "available_parallelism_affinity": int(affinity),
    "os_cpu_count": int(cpu_count),
    "cpu_max": cpu_max,
    "memory_max": memory_max,
    "media_pool_line": media,
    "host_row": json.loads(host_row),
    "cold": {
        "library_db_bytes": int(cold_db),
        "library_db_wal_bytes": int(cold_wal),
        "object_root_bytes": int(cold_objects),
        "object_count": int(cold_object_count),
    },
    "windows": {
        "idle": {"startup_ms": int(startup_ms), **json.loads(idle)},
        "api": {
            "memory_peak_reset": peak_reset_api == "1",
            "memory_peak_label": "window" if peak_reset_api == "1" else "lifetime",
            "per_route": load(routes),
            "mix_60s": load(mix),
            "sample": json.loads(api_sample),
            "library_db_bytes": int(db_after_api),
            "library_db_wal_bytes": int(wal_after_api),
            "scratch_before": json.loads(scratch_before),
        },
        "api_with_rebuild": {
            "memory_peak_reset": peak_reset_rebuild == "1",
            "memory_peak_label": "window" if peak_reset_rebuild == "1" else "lifetime",
            "mix_60s": load(mix_rebuild),
            "sample": json.loads(rebuild_sample),
            "indexed": indexed_n,
            "elapsed_ms": elapsed,
            "books_per_ms": (indexed_n / elapsed) if elapsed else None,
            "scratch_before": json.loads(scratch_before_rebuild),
            "scratch_after": json.loads(scratch_after),
        },
    },
}
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, indent=2)
    fh.write("\n")
print(f"wrote {path}")
PY

echo "envelope metrics written to ${METRICS}"
