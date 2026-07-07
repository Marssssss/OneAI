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
    sourceSets {
        getByName("main") {
            // UniFFI-generated bindings staged by build_android.sh.
            kotlin.srcDirs("src/main/kotlin")
        }
    }
}

dependencies {
    // UniFFI Kotlin bindings use JNA under the hood.
    implementation("net.java.dev:jna:5.13.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1")
}
