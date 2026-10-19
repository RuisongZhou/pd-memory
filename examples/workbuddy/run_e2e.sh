#!/usr/bin/env bash
# One-command reproducible pd-memory <-> WorkBuddy end-to-end run.
#
#   bash examples/workbuddy/run_e2e.sh              # build + simulator (+ real CLI if installed)
#   bash examples/workbuddy/run_e2e.sh --verbose    # echo raw command output
#   bash examples/workbuddy/run_e2e.sh --no-real    # contract simulator only
#   bash examples/workbuddy/run_e2e.sh --only-real  # real headless WorkBuddy CLI only
#
# Output is streamed and additionally written to examples/workbuddy/EVIDENCE.log
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SDK_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$SDK_ROOT"

WORKBUDDY_CLI=/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy
LOG="$SCRIPT_DIR/EVIDENCE.log"

HAS_ROUTE_FLAG=0
ARGS=()
for arg in "$@"; do
  case "$arg" in
    --real|--only-real) HAS_ROUTE_FLAG=1 ;;
    --no-real|--help|-h) HAS_ROUTE_FLAG=1 ;;
  esac
  if [[ "$arg" != "--no-real" ]]; then
    ARGS+=("$arg")
  fi
done

# Default to both routes when the real WorkBuddy CLI is installed; the harness
# itself gracefully SKIPs route B when its CLI is not found.
if [[ "$HAS_ROUTE_FLAG" -eq 0 && -x "$WORKBUDDY_CLI" ]]; then
  ARGS+=(--real)
fi

echo "== pd-memory WorkBuddy e2e =="
echo "sdk root : $SDK_ROOT"
echo "log      : $LOG"
if [[ "${#ARGS[@]}" -gt 0 ]]; then
  echo "args     : ${ARGS[*]}"
else
  echo "args     : <none>"
fi
echo

echo "[run_e2e] cargo build -p pd-memory-cli"
cargo build -p pd-memory-cli

mkdir -p "$SCRIPT_DIR/.run"
run_sim() {
  python3 examples/workbuddy/simulate_workbuddy.py ${ARGS[@]+"${ARGS[@]}"}
}

set +e
run_sim 2>&1 | tee "$LOG"
rc=${PIPESTATUS[0]}
set -e

echo
if [[ "$rc" -eq 0 ]]; then
  echo "[run_e2e] PASS (exit 0) - evidence log: $LOG"
else
  echo "[run_e2e] FAIL (exit $rc) - see $LOG"
fi
exit "$rc"
