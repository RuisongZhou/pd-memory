# pd-memory-ffi — C ABI + JNI bindings

Thin, stable JSON-in / JSON-out bindings over `pd-memory-core`, so a native host
(C/C++) or an Android app (Kotlin/Java) can embed the memory engine.

## Surfaces

| Surface | Entry points |
|---|---|
| C ABI | `pd_memory_version`, `pd_memory_new`, `pd_memory_call`, `pd_memory_last_error`, `pd_memory_string_free`, `pd_memory_free` — see [`include/pd_memory.h`](include/pd_memory.h) |
| JNI | `com.pilotdeck.memory.PdMemoryBridge`: `newEngine`, `callEngine`, `freeEngine`, `lastError`, `version` — see `src/jni_bridge.rs` |

Both surfaces share one dispatch table (`src/dispatch.rs`), so errors, op names
and result shapes are identical. Operations are case/separator-insensitive:
`add` = `capture_turn` = `captureTurn`.

Envelope:

```json
{"ok": true,  "result": { ... }}
{"ok": false, "code": "invalid_config", "message": "data_root must not be empty"}
```

Error codes come from `MemoryError::code()` (`invalid_config`, `io`,
`corrupt_data`, `model`, `model_output`, `json`, `path_escape`, `not_found`,
`unsupported`, `locked`, `invalid_input`, `other`) plus FFI-level codes
(`unknown_op`, `null_pointer`, `null_handle`, `panic`).

## Host build & test

```bash
cd pd-memory
cargo test -p pd-memory-ffi          # FFI smoke tests, no JVM needed
cargo build -p pd-memory-ffi         # target/*/libpd_memory_ffi.{so,dylib,a}
cargo build -p pd-memory-ffi --features http-model   # + real HTTP model providers
```

A ready-made C host smoke test lives in
[`examples/c_host_smoke.c`](examples/c_host_smoke.c); its header comment has the
`cc` build line. On a host with a C compiler it exercises `new -> overview ->
add -> invalid op -> last_error -> free` against `libpd_memory_ffi`.

The default build is **model-free**: it does not link `pd-memory-llm` and
therefore no `ureq`/`rustls`/`ring` C code. Capture/list/get/overview/export/
settings work; retrieve/flush/dream return a `model`/`unsupported` error until
the engine is created with a model provider compiled in (`http-model`). This
split is what lets the crate compile for `*-linux-android` with a plain
`cargo check` on machines without the Android NDK.

## Android

The library `.so` is built per ABI by `scripts/android-build.sh` (`cargo-ndk`)
and packaged by the Gradle sample in [`android/`](../../android/README.md), whose
Kotlin wrapper is `com.pilotdeck.memory.PdMemory`. JNI entry points are
`Java_com_pilotdeck_memory_PdMemoryBridge_*` symbols; the Java declarations are
`static native`, so the second JNI argument is the class reference.

## Safety / lifecycle

* Handles are opaque, owned pointers created by `pd_memory_new` and destroyed by
  exactly one `pd_memory_free` (JNI: `freeEngine`). `NULL`/`0` frees are no-ops.
* Strings returned by `pd_memory_call` / `pd_memory_last_error` are
  heap-allocated (`pd_memory_string_free`); `pd_memory_version` is static.
* The engine is blocking — call from a worker thread, never a UI thread. The
  handle itself is thread-safe for concurrent calls, but freeing must not race.
* Every entry point wraps its body in `catch_unwind`, so Rust panics cannot
  unwind across C/JNI (release builds use `panic = "abort"`, where a panic aborts
  the process instead; engine code is expected to return errors).
