#!/usr/bin/env bash
# Alternate entry point for the production installer.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$SCRIPT_DIR/../../scripts/workbuddy/install.sh" "$@"
