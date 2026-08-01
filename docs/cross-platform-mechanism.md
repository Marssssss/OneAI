# OneAI 跨平台机制

> 一套 Rust 内核（`oneai-core` + `oneai-app`），通过两条 FFI 通路打到六端原生 App。

## 职责

让同一份引擎逻辑在 macOS / Windows / Linux / Android / iOS / HarmonyOS 上以**原生 App**形态运行——不是 WebView 套壳。两条 FFI 通路：

- **UniFFI 绑定**（Kotlin / Swift / Python）—— Android、Apple 平台。
- **手写 `extern "C"` JSON facade**（`crates/oneai-uniffi/src/c_facade.rs`，头文件 `bindings/c/oneai_c.h`）—— 因 `uniffi-bindgen` 0.32 无 C#/ArkTS 生成器，Windows（C# P/Invoke `oneai.dll`）与 HarmonyOS（NAPI 包裹）复用此 facade。所有字符串以 UTF-8 过界，CJK 正确往返。

> extern C 传 String 必须 `CString::new().as_ptr()`（NUL 结尾），`String::as_ptr` 非 NUL 会导致 CStr 越界 UB。

## 六端一览

| 平台 | 技术 | 绑定语言 | 原生 InteractionGate |
|---|---|---|---|
| macOS | SwiftUI（`swiftc`，无需 Xcode） | Swift（UniFFI） | NSAlert |
| Windows | WinUI 3 / C# | C#（P/Invoke facade） | MessageBox |
| Linux | 桌面平台 crate | C++（facade） | MessageBox |
| Android | Jetpack Compose / Kotlin | Kotlin（UniFFI） | AlertDialog |
| iOS | SwiftUI / Swift | Swift（UniFFI xcframework） | UIAlertController |
| HarmonyOS | ArkTS / ArkUI + NAPI | C++（NAPI 包裹 facade） | CommonDialog |

各端共享同一设计：场景化多 Agent 群聊（5 内置预设）、流式 20fps 合并渲染、Markdown、暗色跟随系统、命令面板、产物画布。**macOS App 是参考实现，其他端镜像之。**

## 构建脚本

| 平台 | 脚本 |
|---|---|
| Apple（macOS + iOS xcframework） | `./scripts/build_apple.sh`、`./platforms/macos/build_macos.sh` |
| Windows | `./scripts/build_windows.ps1` |
| Android（4 ABI） | `./scripts/build_android.sh` |
| HarmonyOS | `./scripts/build_harmony.sh` |
| 绑定生成 | `./scripts/generate_bindings.sh {swift|...}` |

各端详细构建步骤见 `platforms/{macos,windows,android,harmony}/README.md`。

## 关键类型与文件

| 项 | 位置 |
|---|---|
| UniFFI 绑定定义 + extern C facade | `crates/oneai-uniffi/src/{c_facade,app_builder,app,group_chat,callback,types}.rs` |
| C 头文件 | `bindings/c/oneai_c.h` |
| 平台 Gate 适配 | `crates/oneai-platform-{desktop,android,ios,harmony}/src/` |
| 桌面平台桥（macOS/Win/Linux） | `crates/oneai-platform-desktop/src/{macos,windows,linux,bridge_common}.rs` |
| Android JNI 桥 | `crates/oneai-platform-android/src/{jni_bridge,gate}.rs` |

## 深入阅读

- [CLAUDE.md — 跨平台 / Network proxy](../CLAUDE.md)
- 平台原生 UI 实现见各 `platforms/*/` 工程
- 权限 Gate 见 [权限机制](permission-mechanism.md)
