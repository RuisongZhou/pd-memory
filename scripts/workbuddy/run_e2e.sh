#!/usr/bin/env bash
# Thin wrapper so `bash scripts/workbuddy/run_e2e.sh` also works.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$SCRIPT_DIR/../../examples/workbuddy/run_e2e.sh" "$@"
