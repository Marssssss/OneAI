plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "ai.oneai"
    compileSdk = 34

    defaultConfig {
        applicationId = "ai.oneai"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.2.0"
    }

    buildTypes {
        getByName("release") {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }

    // .so files staged by scripts/build_android.sh into src/main/jniLibs/<abi>/
    // are packaged automatically — no externalNativeBuild (we build Rust separately).
    //
    // useLegacyPackaging=true extracts .so to disk at install (vs mmap-from-APK).
    // Required for JNA's libjnidispatch.so on 16KB-page Android 15+ images:
    // AGP 8.5 stores the zip entry 8KB-aligned, which the 16KB-page linker rejects
    // ("program alignment cannot be smaller than system page size"). Extracting
    // to disk sidesteps the zip-alignment requirement. (Alt fix: bump AGP ≥8.6.)
    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }
    sourceSets {
        getByName("main") {
            // UniFFI-generated bindings staged by build_android.sh.
            kotlin.srcDirs("src/main/kotlin")
        }
    }
}

dependencies {
    // UniFFI Kotlin bindings use JNA under the hood.
    // NB: groupId is `net.java.dev.jna` (not `net.java.dev` — easy to misread).
    // 5.19.1 required: on Android 16 (SDK 37) 16KB-page images, even JNA 5.16.0's
    // 16KB-aligned libjnidispatch.so is rejected by the linker ("program alignment
    // cannot be smaller than system page size"); 5.19.1's prebuilt passes.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1")
}
