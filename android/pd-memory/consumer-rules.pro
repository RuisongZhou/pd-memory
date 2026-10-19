# Keep the JNI entry points implemented in Rust (crates/pd-memory-ffi/src/jni_bridge.rs).
-keepclasseswithmembernames class com.pilotdeck.memory.PdMemoryBridge {
    native <methods>;
}
