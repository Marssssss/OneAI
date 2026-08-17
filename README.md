# OneAI

[English](README_EN.md) | **简体中文**

> **One AI, Every Platform** —— 跨平台 AI Agent 框架，基于 Rust。一套引擎喂六端前端。

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![CI](https://github.com/Marssssss/OneAI/actions/workflows/ci.yml/badge.svg)](https://github.com/Marssssss/OneAI/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/oneai-app.svg)](https://crates.io/crates/oneai-app)
[![Crates: 31](https://img.shields.io/badge/Crates-31-orange.svg)]()
[![Tests: 2100+](https://img.shields.io/badge/Tests-2100%2B-green.svg)]()
[![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-blue.svg)]()
[![Rust: edition 2021](https://img.shields.io/badge/Rust-edition%202021-dea584.svg)]()
[![Platforms: 6](https://img.shields.io/badge/Platforms-macOS%20%7C%20Win%20%7C%20Linux%20%7C%20Android%20%7C%20iOS%20%7C%20HarmonyOS-blue.svg)]()

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/oneai-logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="assets/oneai-logo.png">
    <img src="assets/oneai-logo.png" alt="OneAI" height="96">
  </picture>
</p>

<p align="center">
  <img src="assets/oneai-webui.png" alt="OneAI WebUI — 浏览器前端" width="860">
</p>

<p align="center"><em>WebUI（<code>oneai web</code> / <code>npx oneai-cli web</code>）· 一行起引擎 + 浏览器前端 + 自动开页。</em></p>

---

## Highlights

- **多类官方前端** —— WebUI、CLI/TUI、macOS / Windows App、VS Code 扩展、浏览器扩展、移动 App，一套引擎喂全端，各端用各端的原生 UI。
- **统一引擎总线** —— 一套 `Directive`（前端→引擎）+ `EngineYield`（引擎→前端）协议，所有前端共用。
- **动态 AgentLoop** —— 每轮模型动态决策（直接回答 / 工具调用 / 委托子 Agent / 切换范式），迭代上限由 Token 预算约束。
- **DomainPack 一行切领域** —— 7 层声明式配置，可合并、可校验、可共享。
- **场景化 GroupChat** —— 引擎级多角色对话原语，5 个内置预设开箱即用。
- **生产级基础设施** —— ProviderPool 降级链 + SmartRouter 路由 + 限流 / 熔断 / 429 重试。
- **跨会话续接** —— 任务进度以事件日志落盘，新 session 自动 surface 上次未完成工作。

---

## 官方前端

同一套引擎，按你的场景挑一个用。**WebUI 零安装、跨端，是面向大多数用户的主推前端**；其余各端用各端原生 UI。

| 前端 | 状态 | 适合 |
|---|---|---|
| WebUI| ✅ 全平台 | 零安装、跨端、场景化对话|
| CLI / TUI | ✅ 全平台 | 通用 Agentic 编程 |
| macOS App | ✅ | 场景化多 Agent 对话 |
| Windows App | ⚠️ 源码构建 | 场景化多 Agent 对话 |
| VS Code 扩展 | ✅ | 在编辑器里聊 |
| 浏览器扩展 | ✅ macOS/Linux | 在浏览器里聊 |
| Android App | ✅ 源码构建 | 移动端场景对话 |
| iOS App | 🚧 在建 | 移动端 |
| HarmonyOS App | 🚧 在建 | 移动端 |

### 1. WebUI

一个命令拉起 Rust 引擎 + React 前端 + 浏览器，跨 macOS / Windows / Linux，零 Rust、零额外进程。同端口（axum）既托管 SPA 静态资源、又提供 `/ws` JSON-RPC 升级。

**从 npm 运行：**

```bash
npx oneai-cli web          # postinstall 拉预编译引擎二进制，起服务并自动开浏览器
# 或全局装一次：
npm install -g oneai-cli
oneai web
```

默认监听 `http://127.0.0.1:8787`，自动打开浏览器。常用参数：`--no-open`（不开页）、`--port`/`--host`、`--model`、`--domain`、`--dist <path>`（指定 web dist 目录）、`--user`。Provider 配置同 CLI（环境变量或 `~/.oneai/config.toml`）。

**从源码运行：**

```bash
# 1) 引擎二进制（http feature 默认开，oneai web 即内置在 oneai-cli）
cargo build --release -p oneai-cli

# 2) 构建 web 前端 dist（一次；oneai web 会自动探测 ./platforms/web/dist）
cd platforms/web && npm install && npm run build && cd ../..

# 3) 起服务
cargo run -p oneai-cli --release -- web
```

> 前端开发模式（热重载）：`cd platforms/web && npm run dev`（Vite 5173），用 `VITE_APP_SERVER_URL=ws://127.0.0.1:8787/ws` 指向一个独立 `oneai app-server --listen ws://127.0.0.1:8787`。

打开页面 → 设置面板配 Provider（类型 / API Key / Base URL / Model，Ollama 留空 key）→ 选场景预设或自建 → 开聊。机制见 [WebUI 机制](docs/webui-mechanism.md)。

### 2. CLI / TUI

通用 Agentic 编程与任务执行前端，全平台（macOS / Windows / Linux）。

安装：

```bash
npm install -g oneai-cli      # 零 Rust，postinstall 拉预编译二进制
# 或跟最新源码：
cargo install --path examples/cli
```

运行：

```bash
oneai          # 进入交互式 TUI
```

Provider 配置（环境变量，或写进 `~/.oneai/config.toml`）：

```bash
export ONEAI_API_KEY="sk-..."
export ONEAI_BASE_URL="https://api.openai.com/v1"
export ONEAI_MODEL="gpt-4o"
```

完整斜杠命令与子命令见 [CLI 参考](docs/cli-reference.md)。

### 3. macOS App

原生 SwiftUI，不需要装 Xcode，Command Line Tools 即可编译。App 默认走**进程内 FFI**（内嵌 `liboneai.a` 静态库，零进程零 socket，体验最佳）；可选切到 **sidecar** 架构（App 进程 spawn `oneai app-server --listen ipc://…` 子进程，经 JSON-RPC 对话引擎）。

```bash
# 1) 先编引擎 release 二进制——sidecar 架构需要它被打进 .app
#    (Contents/Resources/bin/oneai；不编则 sidecar 退回靠 PATH 上的 oneai)
cargo build --release -p oneai-cli

# 2) 编 staticlib + headers + Swift 绑定
./scripts/build_apple.sh

# 3) 编 .app（会把上面的 oneai 二进制一起 bundle 进去）
./platforms/macos/build_macos.sh

open platforms/macos/build/OneAI.app
```

> 只用默认进程内 FFI、不需要 sidecar 的话，可跳过第 1 步。改了引擎代码后，重跑第 1 步再重跑第 2、3 步（`build_macos.sh` 只 stage 不构建，不重编二进制会打进旧引擎）。切到 sidecar：`defaults write oneai_provider oneai_engine_transport sidecar`（改回 FFI 删该 key 即可）。

打开 App → 设置面板配 Provider（类型 / API Key / Base URL / Model，Ollama 留空 key）→ 侧边栏选场景预设（面试演练 / 语言伙伴 / 辩论赛 / 写作工坊 / 头脑风暴）或自建 → 开聊。

### 4. Windows App

原生 WinUI 3 / C#，需 Visual Studio 含 WindowsAppSDK 1.8 workload。

```powershell
rustup target add x86_64-pc-windows-msvc
powershell ./scripts/build_windows.ps1
dotnet run --project platforms\windows\OneAI\OneAI.csproj -c Debug -r win-x64
```

`-r win-x64` 不可省。配置与 macOS 一致（设置面板配 Provider）。详见 [`platforms/windows/README.md`](platforms/windows/README.md)。

### 5. VS Code 扩展

在编辑器里聊。激活即拉起引擎子进程，崩了自动重启。

> 暂未上架 VS Code 市场，先按下面从源码构建调试。

1. 装引擎到 PATH：

   ```bash
   npm install -g oneai-cli      # 或 cargo install --path examples/cli
   ```

2. 编译扩展：

   ```bash
   cd platforms/vscode
   npm install
   npm run compile
   ```

3. 调试运行：用 VS Code 打开 `platforms/vscode` 文件夹，按 `F5`——VS Code 会另开一个装了本扩展的 Extension Development Host 调试窗口。或命令行一行（在 `platforms/vscode` 目录下）：`code --extensionDevelopmentPath="$PWD"`。
4. 配 Provider：打开 VS Code 设置，填 `oneai.apiKey` / `oneai.baseUrl`（留空走官方端点）/ `oneai.model`（如 `gpt-4o`），`oneai.providerKind` 选 `openai` / `anthropic` / `ollama`（Ollama 留空 key）。`oneai` 不在 PATH 时设 `oneai.oneaiPath` 指向二进制。
5. 跑命令 `OneAI: Open Chat` 开聊。

### 6. 浏览器扩展

Chrome / Firefox，经 native messaging 调引擎。

> 暂未上架 Chrome Web Store / AMO，先按下面 sideload 调试。

1. 装引擎到 PATH：

   ```bash
   npm install -g oneai-cli      # 或 cargo install --path examples/cli
   ```

2. 配 Provider，写进 `~/.oneai/config.toml`（引擎会读）：

   ```toml
   [provider]
   api_key = "sk-..."
   base_url = "https://api.openai.com/v1"
   model = "gpt-4o"
   ```

3. 加载扩展拿 ID：
   - **Chrome**：`chrome://extensions` → 开发者模式 → 加载已解包扩展 → 选 `platforms/browser` → 复制扩展 ID。
   - **Firefox**：`about:debugging` → 临时加载附加组件 → 选 `manifest.json`，ID 为 `oneai@oneai`。
4. 注册 native-messaging host：

   ```bash
   cd platforms/browser
   ./install-host.sh --browser=chrome --ext-id=<上一步的ID>
   ```

5. 打开扩展 popup 即连引擎开聊。

> Windows 的 native-messaging host 打包延后，macOS / Linux 现在可用。

### 7. 移动端

Android App（Jetpack Compose / Kotlin）已跑通，`./scripts/build_android.sh` 跨 4 ABI 编译（需 `cargo-ndk` + Android Studio）。

iOS 与 HarmonyOS 是在建端口，分别需 Xcode / DevEco Studio，装上对应 IDE 重跑构建脚本即可。Linux 无独立原生 App，用 CLI。

---

## 贡献

欢迎贡献！先读 [CONTRIBUTING.md](CONTRIBUTING.md)——本地构建 / 测试命令、crate 分层规则、提 PR 前的自查清单。认领 [`good first issue`](https://github.com/Marssssss/OneAI/labels/good%20first%20issue) 上手；设计讨论走 [GitHub Discussions](https://github.com/Marssssss/OneAI/discussions)。行为准则见 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。

## 许可证

Apache-2.0 — 详情见 [LICENSE](LICENSE)。
