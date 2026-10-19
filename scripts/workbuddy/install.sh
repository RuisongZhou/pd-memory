#!/usr/bin/env bash
# Thin wrapper so `bash scripts/workbuddy/install.sh` also works.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$SCRIPT_DIR/../../examples/workbuddy/install_workbuddy.sh" "$@"
