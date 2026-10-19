# pd-memory on Android — Gradle sample

This directory is a self-contained Android Studio project that packages the
Rust memory SDK (`libpd_memory_ffi.so`) into an Android library (`:pd-memory`)
plus a tiny demo app (`:app`).

```
android/
├── settings.gradle.kts / build.gradle.kts / gradle.properties
├── pd-memory/                        # Android library module
│   ├── build.gradle.kts              # jniLibs srcDirs = src/main/jniLibs
│   └── src/main/
│       ├── AndroidManifest.xml
│       ├── jniLibs/                  # populated by ../../scripts/android-build.sh
│       │   ├── arm64-v8a/libpd_memory_ffi.so
│       │   ├── armeabi-v7a/libpd_memory_ffi.so
│       │   └── x86_64/libpd_memory_ffi.so
│       └── java/com/pilotdeck/memory/
│           ├── PdMemoryBridge.java   # static native JNI declarations
│           ├── PdMemory.kt           # Kotlin wrapper (suspend + Dispatchers.IO)
│           ├── PdMemoryException.kt  # stable error codes
│           ├── PdMemoryProtocol.kt   # envelope/argument codec (pure JVM)
│           └── MemoryRepository.kt   # typed, unit-testable helper
└── app/                              # demo app (capture turn + overview)
    └── src/main/java/com/pilotdeck/memory/demo/MainActivity.kt
```

Rust side: `crates/pd-memory-ffi` (C ABI in `include/pd_memory.h`, JNI in
`src/jni_bridge.rs`, op dispatch in `src/dispatch.rs`).

---

## Prerequisites

| Tool | Version | Notes |
|---|---|---|
| Android Studio | Ladybug (2024.2) or newer | includes the Android SDK; JDK 17 required by AGP 8 |
| Android SDK | Platform 35 | `compileSdk = 35`, `minSdk = 26` |
| Android NDK | **r26+** (r27 recommended) | `ANDROID_NDK_HOME=/path/to/Android/sdk/ndk/27.x` or SDK- Manager install; required to compile `ring` (TLS) and to link the `.so` |
| Rust | stable (1.75+) | `rustup` |
| Rust Android targets | `aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android` | installed by `scripts/android-targets.sh` |
| cargo-ndk | 3.x | `cargo install cargo-ndk` |
| Gradle | 8.9+ | use Android Studio's bundled Gradle, or `gradle wrapper` |

No host C toolchain is needed: `cargo-ndk` points cargo at the NDK's clang.

---

## Build steps

From the repository root:

```bash
# 1. One-time: Rust targets (idempotent)
./scripts/android-targets.sh

# 2. Cross-compile the SDK for all three ABIs and copy the .so files into the
#    library module's jniLibs directory.
./scripts/android-build.sh
#    equivalent command (run from ):
#    cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
#        -o android/pd-memory/src/main/jniLibs \
#        build -p pd-memory-ffi --release --features http-model
#    capture-only build without the HTTP model stack:
#    PD_MEMORY_FFI_FEATURES="" ./scripts/android-build.sh

# 3. Build/run the sample with Android Studio
#    * File > Open... > android
#    * Gradle sync (JDK 17; SDK 35 installed)
#    * Run configuration "app" on a device/emulator (arm64-v8a or x86_64)
#
#    or from the command line (Gradle wrapper not committed):
#    gradle wrapper --gradle-version 8.9   # once
#    ./gradlew :app:assembleDebug
```

The Gradle build picks the native libraries up automatically:
`android { sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs") }` in
`pd-memory/build.gradle.kts`; the demo app depends on `:pd-memory`, so the
`.so` files are merged into the APK `lib/<abi>/` directory.

JVM-only unit tests (no device, no `.so` needed) for the JSON protocol and the
repository mapping:

```bash
cd android
./gradlew :pd-memory:testDebugUnitTest
```

---

## Kotlin API

```kotlin
import com.pilotdeck.memory.MemoryRepository
import com.pilotdeck.memory.PdMemory

val memory = PdMemory.create(
    """
    {
      "data_root": "$filesDir/pd-memory",
      "workspace_id": "my-project",
      "workspace_name": "My project"
    }
    """.trimIndent()
)

// Capture (never calls the model; cheap, safe on every turn):
memory.addTurn("user", "The build is cargo-based.", sessionId = "session-1")

// Model-dependent (needs model config + --features http-model):
val overview = memory.overview()
val context = memory.memoryContext("how do we build?")   // null when nothing matched
val results = memory.retrieve("build instructions")
val stats = memory.flush(reason = "manual")

memory.close()   // idempotent; no calls allowed afterwards
```

`PdMemory` exposes the whole op surface (`addTurn`, `retrieve`, `search`,
`memoryContext`, `list`, `get`, `overview`, `flush`, `dream`, `rollback`,
`exportBundle`, `importBundle`, `settings`, `saveSettings`, `act`, `state`).
`MemoryRepository` is a typed convenience layer suitable for view models; it
only needs an `EngineCall` seam, so it is unit-testable without JNI.

### Model configuration

Retrieval/flush/dream need an LLM. Put it in the config JSON:

```json
{
  "data_root": "/data/user/0/<app>/files/pd-memory",
  "workspace_id": "my-project",
  "model": {
    "api_type": "openai_chat",
    "model": "gpt-4o-mini",
    "base_url": "https://api.openai.com/v1",
    "api_key": "sk-..."
  }
}
```

and build the native library with the `http-model` feature (the default of
`scripts/android-build.sh`). If the feature is missing, `PdMemory.create` throws
`PdMemoryException` with code `unsupported`; if no model is configured, capture
and administration work but model-dependent ops fail with code `model`.

> Prefer not to ship long-lived API keys in the APK. A production integration
> usually points `base_url` at your own gateway and injects short-lived tokens.

---

## Threading rules

* The engine is **blocking** (file I/O, optional HTTP). Every `PdMemory` method
  is `suspend` and runs on `Dispatchers.IO`; never call the native engine
  synchronously from the main thread.
* One `PdMemory` instance can be shared by concurrent coroutines: the Rust
  engine serialises its own state. Calls execute in order of contention, not by
  call order.
* `close()` frees the native handle. Call it once the UI/owner is done; it is
  idempotent (the handle field is zeroed under a lock before `freeEngine`).
  Concurrent calls racing with `close()` fail with code `closed`.
* Never call `PdMemoryBridge` directly — it is package-private on purpose.

## Memory / lifecycle notes

* `newEngine` box-allocates the Rust `PdMemoryHandle`; `freeEngine` drops it.
  A leaked handle leaks the engine (and its open file paths) until process exit.
* Java/Kotlin strings returned by JNI are copied into the JVM heap; no manual
  native string freeing is needed on the Kotlin side (the Rust side does its own
  `CString` bookkeeping inside each call).
* `System.loadLibrary("pd_memory_ffi")` loads the ABI-specific `.so` chosen by
  the platform; missing libraries surface as `UnsatisfiedLinkError` on first
  use.
* Captured turns are durable files under `data_root`; `exportBundle()` returns a
  JSON snapshot that can be persisted or transferred via `importBundle()`.

## Ops over the bridge

All ops return `{"ok":true,"result":...}` / `{"ok":false,"code":...,"message":...}`:

| op | args | result |
|---|---|---|
| `add` / `capture_turn` | `{messages:[{role,content}], session_id?, source?, timestamp?}` | `CaptureTurnResult` |
| `retrieve` / `search` | `{query, recent_messages?, workspace_hint?, explicit?}` | `RetrievalResult` |
| `list` | `{kinds?, query?, include_deprecated?, limit?, offset?}` | `[MemoryManifestEntry]` |
| `get` | `{ids:[string], max_lines?}` | `[MemoryFileRecord]` |
| `overview` | `{}` | `MemoryOverview` |
| `flush` | `{batch_size?, session_keys?, reason?}` | `HeartbeatStats` |
| `dream` | `{trigger?: "manual"\|"scheduled"}` | `DreamRunResult` |
| `rollback` | `{}` | `DreamRollbackResult` |
| `export` | `{}` | `MemoryExportBundle` |
| `import` | `{bundle: MemoryExportBundle}` | `MemoryImportResult` |
| `settings_get` | `{}` | `IndexingSettings` |
| `settings_save` | `{settings: IndexingSettings}` | `IndexingSettings` |
| `act` | `{action: MemoryActionRequest}` | `MemoryActionResult` |
| `state_get` | `{key}` | JSON value / `null` |
| `memory_context` | `{query, recent_messages?}` | `{systemContext: string \| null}` |

Result shapes are the `serde` serialisation of the `pd-memory-core` types in
`crates/pd-memory-core/src/types.rs`.

---

## Verification on this machine

Host: **macOS 27.0.1, Apple Silicon (`Darwin arm64`)**; `rustc 1.99.0`,
`cargo 1.99.0`. The machine has **no Android SDK, no NDK and no cargo-ndk**
(`ANDROID_HOME` / `ANDROID_NDK_HOME` unset, `~/Library/Android` absent), so
linking a `.so`, Gradle builds and device runs are impossible here. What *was*
verified for real:

1. the FFI crate type-checks for every shipped ABI **without an NDK**;
2. the C ABI passes its host-side unit tests (10 tests, no JVM);
3. `cargo check --workspace` is green on the host.

### 1. Rust Android targets

```text
$ ./scripts/android-targets.sh
==> workspace: /path/to/pd-memory
==> rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
info: component rust-std for target aarch64-linux-android is up to date
info: component rust-std for target armv7-linux-androideabi is up to date
info: component rust-std for target x86_64-linux-android is up to date
==> installed targets:
    aarch64-apple-darwin
    aarch64-linux-android
    armv7-linux-androideabi
    x86_64-linux-android
```

### 2. Cross-target check (the reproducible NDK-free verification)

```text
$ ./scripts/android-check.sh
...
==> cargo check -p pd-memory-ffi --target aarch64-linux-android
    Checking pd-memory-core v0.1.0 (.../crates/pd-memory-core)
    Checking pd-memory-ffi v0.1.0 (.../crates/pd-memory-ffi)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.53s

==> cargo check -p pd-memory-ffi --target armv7-linux-androideabi
    Checking pd-memory-core v0.1.0 (.../crates/pd-memory-core)
    Checking pd-memory-ffi v0.1.0 (.../crates/pd-memory-ffi)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.02s

==> cargo check -p pd-memory-ffi --target x86_64-linux-android
    Checking pd-memory-core v0.1.0 (.../crates/pd-memory-core)
    Checking pd-memory-ffi v0.1.0 (.../crates/pd-memory-ffi)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.81s

==> android-check summary
    PASS  cargo check -p pd-memory-ffi --target aarch64-linux-android
    PASS  cargo check -p pd-memory-ffi --target armv7-linux-androideabi
    PASS  cargo check -p pd-memory-ffi --target x86_64-linux-android
==> all 3 Android targets compile (default features, no NDK needed)
```

The default feature set deliberately excludes the HTTP model stack, so the
Android dependency graph contains no native C/asm build:

```text
$ cargo tree -p pd-memory-ffi --target aarch64-linux-android -e normal \
    | grep -E 'ureq|rustls|ring|pd-memory-llm'
(no output)
```

Enabling `http-model` (needed for real LLM calls) *does* require the NDK — this
is the expected failure on this machine and the reason the feature is opt-in:

```text
$ cargo check -p pd-memory-ffi --target aarch64-linux-android --features http-model
   Compiling ring v0.17.14
error: failed to run custom build command for `ring v0.17.14`
cargo:warning=Compiler family detection failed ... failed to find tool
  "aarch64-linux-android-clang": No such file or directory (os error 2)
```

### 3. Host FFI tests (C ABI driven directly, no JVM)

```text
$ cargo test -p pd-memory-ffi
running 10 tests
test model::tests::configured_model_needs_http_model_feature ... ok
test model::tests::empty_model_config_uses_fallback_provider ... ok
test tests::invalid_arguments_are_reported_without_panicking ... ok
test tests::invalid_config_returns_null_with_thread_local_error ... ok
test tests::model_configured_without_feature_is_rejected ... ok
test tests::non_utf8_op_name_is_rejected ... ok
test tests::null_handles_are_safe ... ok
test tests::smoke_new_overview_add_list_export_free ... ok
test tests::unknown_op_returns_error_envelope_and_records_last_error ... ok
test tests::version_is_a_static_utf8_string ... ok
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Covered: `new -> overview -> add (capture_turn alias) -> list/get -> settings
-> export -> state_get -> free`, unknown op, invalid args JSON, invalid config,
NULL handle/config/string frees, invalid UTF-8 input, panic-free JNI-less path.

### 3b. Real C host smoke test (same C ABI, compiled from C)

```text
$ cargo build -p pd-memory-ffi
$ cc -Wall -Wextra -I crates/pd-memory-ffi/include \
      crates/pd-memory-ffi/examples/c_host_smoke.c \
      -L target/debug -lpd_memory_ffi -Wl,-rpath,"$PWD/target/debug" \
      -o /tmp/pd_memory_smoke
$ PD_ROOT=$(mktemp -d) /tmp/pd_memory_smoke
version=0.1.0
overview={"ok":true,"result":{"changed_files_since_last_dream":0,...,
          "total_files":0,"workspace_mode":"single","workspace_name":"c-smoke"}}
add={"ok":true,"result":{"captured":true,"normalized_messages":[
     {"content":"C host smoke test","role":"user"}],"session_key":"c-session"}}
invalid_op={"code":"unknown_op","message":"unknown op `notanop`; supported ops: add
             (alias capture_turn), retrieve, ...","ok":false}
last_error_non_empty=1
```

This exercises the exported symbols from a real C program (header, envelope
parsing, error path, `last_error`, `string_free`, `free`) on the host. The JNI
entry points are the same dispatch path but were not invoked (no JVM here).

### 3c. Exported symbols (host `cdylib`)

```text
$ nm -gU target/debug/libpd_memory_ffi.dylib | grep -E 'pd_memory_|PdMemoryBridge'
_pd_memory_call
_pd_memory_free
_pd_memory_last_error
_pd_memory_new
_pd_memory_string_free
_pd_memory_version
_Java_com_pilotdeck_memory_PdMemoryBridge_callEngine
_Java_com_pilotdeck_memory_PdMemoryBridge_freeEngine
_Java_com_pilotdeck_memory_PdMemoryBridge_lastError
_Java_com_pilotdeck_memory_PdMemoryBridge_newEngine
_Java_com_pilotdeck_memory_PdMemoryBridge_version
```

`cargo build -p pd-memory-ffi` also emits the `staticlib`
(`target/debug/libpd_memory_ffi.a`) for native hosts.

### 4. Workspace + formatting

```text
$ cargo check --workspace
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.15s

$ cargo fmt -p pd-memory-ffi -- --check
(clean)
```

`cargo fmt --all -- --check` currently reports diffs only in crates that are
being implemented by other teammates in parallel (`pd-memory-core`,
`pd-memory-sqlite`, `pd-memory-testkit`); `pd-memory-ffi` itself is formatted.
Run `cargo fmt --all` once the whole workspace has landed.

## What is NOT verified here / remaining work

* **Linking the `.so`** (`cargo ndk ... build --release --features http-model`):
  requires NDK r26+. `scripts/android-build.sh` is ready but was **not**
  executed on this machine.
* **Gradle/AGP build** (`./gradlew :app:assembleDebug`): no Android SDK/JDK
  installed. The Kotlin/Java sources target AGP 8.7.3, Kotlin 2.0.21, SDK 35 and
  JDK 17 but were not compiled here; treat them as a sample to validate in
  Android Studio.
* **JNI symbols on a real JVM/ART**: the Rust JNI functions compile for host and
  Android, but no JVM/emulator was available to invoke them. First on-device
  smoke test should be `PdMemory.version()` then `PdMemory.create(...)`.
* **ABI loading / packaging**: `System.loadLibrary("pd_memory_ffi")`, the
  `lib/<abi>/` APK layout, armv7 runtime behaviour, and the demo Activity flow.
* **`http-model` runtime**: TLS/network on Android and real model calls
  (retrieve/flush/dream) are untested.
* **JVM unit tests** under `android/pd-memory/src/test/` were not executed
  (no JDK/Gradle); they are pure-JVM and only need `org.json` + JUnit +
  `kotlinx-coroutines-test`, so Android Studio can run them without a device.
* **Gradle wrapper** is not committed (no binary jar); use Android Studio's
  Gradle or `gradle wrapper --gradle-version 8.9` once.
* **Release profile caveat**: the workspace sets `panic = "abort"` for release,
  so the `catch_unwind` guards in the FFI are effective in dev/test profiles but
  a panic in a release `.so` aborts the process. Engine code is written to return
  `MemoryError` instead of panicking.

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| `UnsatisfiedLinkError: could not find "libpd_memory_ffi.so"` | Run `./scripts/android-build.sh`; check `android/pd-memory/src/main/jniLibs/<abi>/libpd_memory_ffi.so` exists for the device ABI; clean/rebuild the app. |
| `cargo ndk` fails with `failed to find tool "aarch64-linux-android-clang"` | NDK not configured: set `ANDROID_NDK_HOME` (r26+) and re-run. |
| `error: linker cc not found` / `ring` build failures | Same cause: the NDK is required to build the `http-model` stack; install NDK r26+ or build with `PD_MEMORY_FFI_FEATURES=""`. |
| `unsupported: a model is configured but this build does not include the HTTP model providers` | Rebuild with `--features http-model` (the default of `scripts/android-build.sh`). |
| `model: no model provider is configured` | Add a `model` object to the config JSON (capture-only configs cannot retrieve). |
| Gradle sync fails on JDK | Use JDK 17 (Android Studio: Settings > Build Tools > Gradle > Gradle JDK). |
| `Structure of the app` shows no `.so` | jniLibs path is relative to the module: `android/pd-memory/src/main/jniLibs`. |
