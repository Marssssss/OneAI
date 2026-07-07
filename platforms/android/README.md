# OneAI Android

Minimal Android app shell that loads `liboneai.so` and exercises the UniFFI
FFI surface (built in S2; full chat UI lands in S3+).

## Build

The native `.so` and the UniFFI-generated Kotlin bindings are **not**
committed — `scripts/build_android.sh` cross-compiles and stages them:

```bash
# from repo root
./scripts/build_android.sh            # 4 ABIs, release
cd platforms/android
./gradlew assembleDebug               # produces app/build/outputs/apk/debug/app-debug.apk
./gradlew installDebug                # install on a connected device/emulator
```

Prerequisites (one-time):
- Android NDK — `brew install --cask android-ndk` (or Android Studio SDK Manager)
- `cargo install cargo-ndk`
- `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android`

The smoke `MainActivity` calls `OneAiAppBuilder().build()` → `createSession()`
→ `sessionId()` / `platform()` over FFI — watch logcat tag `OneAI`.

> JNA (`net.java.dev:jna`) is resolved from Maven Central; ensure your
> network/proxy can reach `repo.maven.apache.org`.
