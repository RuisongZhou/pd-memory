#!/usr/bin/env bash
#
# Install the Rust standard-library targets needed to build/check pd-memory for
# Android. Safe to re-run; rustup is idempotent.
#
# Usage:  ./scripts/android-targets.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

RUSTUP="${RUSTUP:-}"
if [[ -z "$RUSTUP" ]]; then
    RUSTUP="$(command -v rustup || true)"
fi
if [[ -z "$RUSTUP" && -x "$HOME/.cargo/bin/rustup" ]]; then
    RUSTUP="$HOME/.cargo/bin/rustup"
fi
if [[ -z "$RUSTUP" ]]; then
    echo "error: rustup not found; install it from https://rustup.rs" >&2
    exit 1
fi

TARGETS=(
    aarch64-linux-android      # arm64-v8a   (phones, most devices)
    armv7-linux-androideabi    # armeabi-v7a (older 32-bit devices)
    x86_64-linux-android       # x86_64      (emulators / ChromeOS)
)

echo "==> workspace: $ROOT"
echo "==> rustup target add ${TARGETS[*]}"
"$RUSTUP" target add "${TARGETS[@]}"

echo "==> installed targets:"
"$RUSTUP" target list --installed | sed 's/^/    /'
