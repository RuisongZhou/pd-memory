// Root Gradle build for the pd-memory Android sample.
//
// The sample wires the Rust SDK (`libpd_memory_ffi.so`) into an Android library
// module (`:pd-memory`) and a tiny demo app (`:app`). The native libraries are
// produced by ../../scripts/android-build.sh via cargo-ndk and copied to
// pd-memory/src/main/jniLibs/<abi>/libpd_memory_ffi.so.
plugins {
    id("com.android.application") version "8.7.3" apply false
    id("com.android.library") version "8.7.3" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
}
