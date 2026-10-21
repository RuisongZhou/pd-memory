#!/usr/bin/env bash
# Build and install pd-memory into an agent host; preview unless --apply is given.
# Usage: bash scripts/install.sh --host <workbuddy|codex|deepseek-harness> [--apply] [options]
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$SCRIPT_DIR/install/install.py" "$@"
