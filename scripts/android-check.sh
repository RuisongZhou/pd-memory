#!/usr/bin/env bash
#
# Verify that pd-memory-ffi compiles for every Android ABI the sample ships.
#
# This is the CI-friendly check for machines WITHOUT an Android SDK/NDK:
# pd-memory-ffi's default feature set is model-free (the HTTP model stack
# ureq -> rustls -> ring compiles C/asm and would need the NDK). Host `cargo
# test -p pd-memory-ffi` exercises the same code paths natively.
#
# Building without the NDK is not a project requirement: Android support only
# has to build and run with one. This script calls plain `cargo check`, so once
# a dependency that compiles C/C++ enters the default graph it needs the NDK's
# clang: export CC_<target>/AR_<target> pointing at the NDK toolchain, or run
# `cargo ndk -t <abi> check -p pd-memory-ffi` instead. scripts/android-build.sh
# is the full build.
#
# Usage:  ./scripts/android-check.sh
# Exit code: 0 only when all three target checks pass.
#
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CARGO="${CARGO:-}"
if [[ -z "$CARGO" ]]; then
    CARGO="$(command -v cargo || true)"
fi
if [[ -z "$CARGO" && -x "$HOME/.cargo/bin/cargo" ]]; then
    CARGO="$HOME/.cargo/bin/cargo"
fi
if [[ -z "$CARGO" ]]; then
    echo "error: cargo not found; install Rust from https://rustup.rs" >&2
    exit 1
fi

"$ROOT/scripts/android-targets.sh"

TARGETS=(
    aarch64-linux-android
    armv7-linux-androideabi
    x86_64-linux-android
)

cd "$ROOT"

status=0
passed=()
failed=()

for target in "${TARGETS[@]}"; do
    echo
    echo "==> cargo check -p pd-memory-ffi --target $target"
    if "$CARGO" check -p pd-memory-ffi --target "$target"; then
        passed+=("$target")
    else
        failed+=("$target")
        status=1
    fi
done

echo
echo "==> android-check summary"
for target in "${passed[@]:-}"; do
    [[ -n "$target" ]] && echo "    PASS  cargo check -p pd-memory-ffi --target $target"
done
for target in "${failed[@]:-}"; do
    [[ -n "$target" ]] && echo "    FAIL  cargo check -p pd-memory-ffi --target $target"
done

if [[ "$status" -eq 0 ]]; then
    echo "==> all ${#TARGETS[@]} Android targets compile (default features, no NDK needed)"
else
    echo "==> ${#failed[@]} target(s) failed; see output above" >&2
fi

exit "$status"
