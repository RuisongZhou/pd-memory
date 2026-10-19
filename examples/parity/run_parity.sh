#!/usr/bin/env bash
# Differential memory-parity harness: PilotDeck TypeScript memory core vs the
# Rust pd-memory SDK, driven by identical deterministic scenarios and one
# deterministic mock model server.
#
#   PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh                 # all scenarios
#   PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh --scenario P5   # one scenario
#   PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh --no-ts         # Rust-only smoke
#   PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh --verbose       # full diff detail
#
# Exit code is 0 only when every compared scenario has no unexpected DIFF.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SDK_ROOT="$(cd "$HERE/../.." && pwd)"
REPO_ROOT="${PILOTDECK_ROOT:-$(cd "$SDK_ROOT/../.." && pwd)}"
RUN_ROOT="$HERE/.run"
BINARY="$SDK_ROOT/target/debug/pd-memory"
SCENARIOS_FILE="$HERE/scenarios.json"
PY=python3

SELECTED=()
NO_TS=0
NO_BUILD=0
VERBOSE=""

usage() {
  sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --scenario)
      shift
      [[ $# -gt 0 ]] || { echo "error: --scenario needs a value" >&2; exit 2; }
      IFS=',' read -r -a parts <<< "$1"
      SELECTED+=("${parts[@]}")
      ;;
    --scenario=*)
      IFS=',' read -r -a parts <<< "${1#--scenario=}"
      SELECTED+=("${parts[@]}")
      ;;
    --no-ts) NO_TS=1 ;;
    --no-build) NO_BUILD=1 ;;
    --verbose) VERBOSE="--verbose" ;;
    --help|-h) usage; exit 0 ;;
    *) echo "error: unknown argument $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [[ "$NO_TS" -eq 0 && ! -f "$REPO_ROOT/src/context/memory/edgeclaw-memory-core/lib/index.js" ]]; then
  echo "error: set PILOTDECK_ROOT to a PilotDeck checkout with edgeclaw-memory-core/lib/index.js (see docs/parity.md)" >&2
  exit 2
fi

if [[ ${#SELECTED[@]} -eq 0 ]]; then
  # bash 3.2 (macOS) has no `mapfile`; read the scenario names line by line.
  while IFS= read -r scenario_name; do
    [[ -n "$scenario_name" ]] && SELECTED+=("$scenario_name")
  done < <($PY - "$SCENARIOS_FILE" <<'PY'
import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
for name in data["scenarios"]:
    print(name)
PY
)
fi

mock_scenario_for() {
  $PY - "$SCENARIOS_FILE" "$1" <<'PY'
import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
print(data["scenarios"][sys.argv[2]].get("mock", "default"))
PY
}

scenario_title_for() {
  $PY - "$SCENARIOS_FILE" "$1" <<'PY'
import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
print(data["scenarios"][sys.argv[2]].get("title", ""))
PY
}

MOCK_PID=""
cleanup() {
  if [[ -n "$MOCK_PID" ]] && kill -0 "$MOCK_PID" 2>/dev/null; then
    kill "$MOCK_PID" 2>/dev/null
    wait "$MOCK_PID" 2>/dev/null
  fi
  MOCK_PID=""
}
trap cleanup EXIT INT TERM

if ! "$PY" -m unittest discover -s "$HERE" -p 'test_*.py' -q; then
  echo "error: parity comparator regression checks failed" >&2
  exit 2
fi

mkdir -p "$RUN_ROOT"
rm -rf "$RUN_ROOT/scenarios" "$RUN_ROOT/report.json" "$RUN_ROOT/environment.json"
mkdir -p "$RUN_ROOT/scenarios"

# ---------------------------------------------------------------------------
# Build the Rust CLI once (incremental cargo build; no network needed).
# ---------------------------------------------------------------------------
if [[ "$NO_BUILD" -eq 0 ]]; then
  echo "==> building Rust CLI (cargo build -p pd-memory-cli)"
  if ! ( cd "$SDK_ROOT" && cargo build --quiet -p pd-memory-cli --bins --examples ); then
    echo "error: cargo build -p pd-memory-cli failed" >&2
    exit 2
  fi
fi
if [[ ! -x "$BINARY" || ! -x "$SDK_ROOT/target/debug/examples/parity-runtime" ]]; then
  echo "error: Rust CLI not found at $BINARY (run without --no-build)" >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# Environment snapshot (reproducibility section of docs/parity.md).
# ---------------------------------------------------------------------------
$PY - "$REPO_ROOT" "$BINARY" "$RUN_ROOT/environment.json" <<'PY'
import hashlib, json, subprocess, sys, platform
from pathlib import Path

repo_root, binary, out = sys.argv[1], sys.argv[2], sys.argv[3]

def run(*cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=30).stdout.strip()
    except Exception:
        return "n/a"

def tree_digest(directory):
    digest = hashlib.sha256()
    root = Path(directory)
    for path in sorted(root.rglob("*")):
        if path.is_file():
            digest.update(path.relative_to(root).as_posix().encode())
            digest.update(b"\0")
            digest.update(path.read_bytes())
    return digest.hexdigest()

data = {
    "pilotdeckCommit": run("git", "-C", repo_root, "rev-parse", "--short", "HEAD"),
    "pilotdeckCommitFull": run("git", "-C", repo_root, "rev-parse", "HEAD"),
    "upstreamSourceDigest": tree_digest(Path(repo_root) / "src/context/memory/edgeclaw-memory-core/src"),
    "upstreamLibDigest": tree_digest(Path(repo_root) / "src/context/memory/edgeclaw-memory-core/lib"),
    "node": run("node", "--version"),
    "python": platform.python_version(),
    "cargo": run("cargo", "--version"),
    "rustc": run("rustc", "--version"),
    "platform": platform.platform(),
    "rustCli": binary,
    "rustCliVersion": run(binary, "--version"),
}
open(out, "w", encoding="utf-8").write(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
PY

# ---------------------------------------------------------------------------
# Run every selected scenario.
# ---------------------------------------------------------------------------
declare -a SUMMARY
FAILED=0
DIFFED=0
SKIPPED=0

for SCENARIO in "${SELECTED[@]}"; do
  MOCK_SCENARIO="$(mock_scenario_for "$SCENARIO")"
  TITLE="$(scenario_title_for "$SCENARIO")"
  RUN_DIR="$RUN_ROOT/scenarios/$SCENARIO"
  mkdir -p "$RUN_DIR/mock" "$RUN_DIR/logs"
  echo ""
  echo "==> scenario $SCENARIO ($TITLE) [mock=$MOCK_SCENARIO]"

  cleanup
  $PY "$HERE/mock_model_server.py" \
    --port 0 \
    --scenario "$MOCK_SCENARIO" \
    --log-dir "$RUN_DIR/mock" \
    --port-file "$RUN_DIR/mock/port" \
    >"$RUN_DIR/logs/mock.log" 2>&1 &
  MOCK_PID=$!

  for _ in $(seq 1 100); do
    [[ -s "$RUN_DIR/mock/port" ]] && break
    sleep 0.05
  done
  if [[ ! -s "$RUN_DIR/mock/port" ]]; then
    echo "error: mock model did not start (see $RUN_DIR/logs/mock.log)" >&2
    cleanup
    SUMMARY+=("$SCENARIO|$MOCK_SCENARIO|mock-failed|n/a|FAILED")
    FAILED=1
    continue
  fi
  PORT="$(cat "$RUN_DIR/mock/port")"
  echo "    mock model on 127.0.0.1:$PORT"

  TS_STATUS="skipped"
  RUST_STATUS="failed"
  if [[ "$NO_TS" -eq 0 ]]; then
    if node "$HERE/ts_runner.mjs" --repo-root "$REPO_ROOT" --scenario "$SCENARIO" --run-dir "$RUN_DIR" --port "$PORT" \
        >"$RUN_DIR/logs/ts.log" 2>"$RUN_DIR/logs/ts.err"; then
      TS_STATUS="ok"
    else
      TS_STATUS="failed"
      echo "error: TS runner failed (see $RUN_DIR/logs/ts.err)" >&2
      tail -n 5 "$RUN_DIR/logs/ts.err" >&2
    fi
  fi
  if $PY "$HERE/rust_runner.py" --scenario "$SCENARIO" --run-dir "$RUN_DIR" --port "$PORT" --binary "$BINARY" \
      >"$RUN_DIR/logs/rust.log" 2>"$RUN_DIR/logs/rust.err"; then
    RUST_STATUS="ok"
  else
    echo "error: Rust runner failed (see $RUN_DIR/logs/rust.err)" >&2
    tail -n 5 "$RUN_DIR/logs/rust.err" >&2
  fi

  cleanup

  echo "    ts=$TS_STATUS rust=$RUST_STATUS; comparing"
  $PY "$HERE/compare.py" --scenario "$SCENARIO" --run-dir "$RUN_DIR" \
    --report "$RUN_DIR/report.json" $VERBOSE
  COMPARE_RC=$?
  VERDICT="$($PY - "$RUN_DIR/report.json" <<'PY'
import json, sys
try:
    report = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception:
    print("ERROR")
    raise SystemExit(0)
print(report.get("verdict", "ERROR"))
PY
)"
  if [[ "$TS_STATUS" == "failed" || "$RUST_STATUS" == "failed" ]]; then
    VERDICT="ERROR"
    FAILED=1
  elif [[ "$COMPARE_RC" -ne 0 && "$VERDICT" != "SKIP" ]]; then
    DIFFED=1
  fi
  if [[ "$VERDICT" == "SKIP" ]]; then
    SKIPPED=$((SKIPPED + 1))
  fi
  SUMMARY+=("$SCENARIO|$MOCK_SCENARIO|$TS_STATUS|$RUST_STATUS|$VERDICT")
done

# ---------------------------------------------------------------------------
# Aggregate report + summary table.
# ---------------------------------------------------------------------------
$PY - "$RUN_ROOT" "$SCENARIOS_FILE" "${SELECTED[@]}" <<'PY'
import json, sys
from pathlib import Path

run_root = Path(sys.argv[1])
scenario_file = Path(sys.argv[2])
selected = sys.argv[3:]
specs = json.loads(scenario_file.read_text(encoding="utf-8"))["scenarios"]
environment_file = run_root / "environment.json"
environment = json.loads(environment_file.read_text(encoding="utf-8")) if environment_file.exists() else {}
scenarios = []
counts = {"PASS": 0, "DIFF": 0, "SKIP": 0, "ERROR": 0}
for name in selected:
    report_file = run_root / "scenarios" / name / "report.json"
    if report_file.exists():
        report = json.loads(report_file.read_text(encoding="utf-8"))
    else:
        report = {"scenario": name, "title": specs.get(name, {}).get("title", ""), "verdict": "ERROR", "counts": {}, "aspects": []}
    report["title"] = specs.get(name, {}).get("title", report.get("title", ""))
    scenarios.append(report)
    verdict = report.get("verdict", "ERROR")
    counts[verdict] = counts.get(verdict, 0) + 1
aggregate = {
    "generatedAt": __import__("datetime").datetime.now().astimezone().isoformat(timespec="seconds"),
    "environment": environment,
    "scenarios": scenarios,
    "summary": counts,
}
(run_root / "report.json").write_text(json.dumps(aggregate, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
PY

echo ""
echo "================ PARITY SUMMARY ================"
printf "%-8s %-8s %-6s %-6s %-8s %s\n" "scenario" "mock" "ts" "rust" "compare" "verdict"
for row in "${SUMMARY[@]}"; do
  IFS='|' read -r name mock ts rust verdict <<< "$row"
  printf "%-8s %-8s %-6s %-6s %-8s %s\n" "$name" "$mock" "$ts" "$rust" \
    "$([[ "$verdict" == "PASS" || "$verdict" == "SKIP" || "$verdict" == "DIFF" ]] && echo ok || echo failed)" \
    "$verdict"
done
echo "-----------------------------------------------"
$PY - "$RUN_ROOT/report.json" <<'PY'
import json, sys
report = json.load(open(sys.argv[1], encoding="utf-8"))
counts = report["summary"]
print(f"PASS={counts.get('PASS', 0)} DIFF={counts.get('DIFF', 0)} SKIP={counts.get('SKIP', 0)} ERROR={counts.get('ERROR', 0)}")
print(f"report: {sys.argv[1]}")
PY

if [[ "$FAILED" -ne 0 ]]; then
  echo "RESULT: FAILED (runner/mock error)" >&2
  exit 2
fi
if [[ "$DIFFED" -ne 0 ]]; then
  echo "RESULT: unexpected DIFF found (see report.json)" >&2
  exit 1
fi
echo "RESULT: no unexpected differences"
exit 0
