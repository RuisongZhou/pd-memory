#!/usr/bin/env bash
# Build and install the WorkBuddy integration; preview unless --apply is given.
# Compatibility shim for `scripts/install.sh --host workbuddy`.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$SCRIPT_DIR/../install.sh" --host workbuddy "$@"
