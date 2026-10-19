plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.pilotdeck.memory"
    compileSdk = 35

    defaultConfig {
        minSdk = 26
        // Consumer apps must declare INTERNET themselves when they configure a
        // remote model provider; the library deliberately does not add it.
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    // libpd_memory_ffi.so is produced by ../../scripts/android-build.sh:
    //   cargo ndk ... -o android/pd-memory/src/main/jniLibs build -p pd-memory-ffi ...
    // The ABI subdirectories (arm64-v8a/, armeabi-v7a/, x86_64/) are picked up
    // automatically and packaged into the AAR/APK.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    buildFeatures {
        buildConfig = false
    }
}

dependencies {
    implementation("androidx.annotation:annotation:1.9.1")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    // Local (JVM) unit tests for the JSON protocol and repository mapping. The
    // real org.json implementation is needed because android.jar's stubs throw
    // "not mocked" outside an emulator.
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.9.0")
}
