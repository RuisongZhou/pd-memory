#!/usr/bin/env bash
#
# Build libpd_memory_ffi.so for Android with cargo-ndk and drop the libraries
# into the Gradle sample (android/pd-memory/src/main/jniLibs/<abi>/), which is
# the library module's default jniLibs source set.
#
# Requires:
#   * Rust + the Android targets (scripts/android-targets.sh installs them)
#   * Android NDK r26+ (ANDROID_NDK_HOME or ANDROID_HOME/ndk/<version>)
#   * cargo-ndk          -> cargo install cargo-ndk
#
# Usage:
#   ./scripts/android-build.sh
#   JNI_LIBS_DIR=/tmp/jniLibs ./scripts/android-build.sh   # custom output dir
#   PD_MEMORY_FFI_FEATURES="" ./scripts/android-build.sh   # model-free build
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
JNI_LIBS_DIR="${JNI_LIBS_DIR:-$ROOT/android/pd-memory/src/main/jniLibs}"
# `http-model` links the real HTTP model providers (ureq/rustls/ring). It needs
# the NDK, which this build has by definition; drop it for a capture-only .so.
FEATURES="${PD_MEMORY_FFI_FEATURES-http-model}"

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

CARGO_NDK="${CARGO_NDK:-}"
if [[ -z "$CARGO_NDK" ]]; then
    CARGO_NDK="$(command -v cargo-ndk || true)"
fi
if [[ -z "$CARGO_NDK" && -x "$HOME/.cargo/bin/cargo-ndk" ]]; then
    CARGO_NDK="$HOME/.cargo/bin/cargo-ndk"
fi
if [[ -z "$CARGO_NDK" ]]; then
    echo "error: cargo-ndk not found; install it with: cargo install cargo-ndk" >&2
    exit 1
fi

if [[ -z "${ANDROID_NDK_HOME:-}${ANDROID_NDK_ROOT:-}${ANDROID_HOME:-}${ANDROID_SDK_ROOT:-}" ]]; then
    echo "warning: no ANDROID_NDK_HOME / ANDROID_HOME found. cargo-ndk needs an NDK r26+;" >&2
    echo "         install it via Android Studio SDK Manager or set ANDROID_NDK_HOME." >&2
fi

"$ROOT/scripts/android-targets.sh"

mkdir -p "$JNI_LIBS_DIR"
cd "$ROOT"

echo
echo "==> cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o $JNI_LIBS_DIR"
echo "        build -p pd-memory-ffi --release --features \"$FEATURES\""

if [[ -n "$FEATURES" ]]; then
    "$CARGO_NDK" ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o "$JNI_LIBS_DIR" \
        build -p pd-memory-ffi --release --features "$FEATURES"
else
    "$CARGO_NDK" ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o "$JNI_LIBS_DIR" \
        build -p pd-memory-ffi --release
fi

echo
echo "==> native libraries written to $JNI_LIBS_DIR"
find "$JNI_LIBS_DIR" -name 'libpd_memory_ffi.so' -print | sort | while read -r so; do
    printf '    %s (%s)\n' "$so" "$(du -h "$so" | cut -f1)"
done

echo
echo "Next: open android in Android Studio (or run ./gradlew :app:assembleDebug)."
