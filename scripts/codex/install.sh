#!/usr/bin/env bash
# Build and install the Codex integration; preview unless --apply is given.
# Compatibility shim for `scripts/install.sh --host codex`.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$SCRIPT_DIR/../install.sh" --host codex "$@"
