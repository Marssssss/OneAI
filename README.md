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
  <img src="assets/OneAI_icon.png" alt="OneAI" width="160">
</p>

---

## 一图看懂

<p align="center">
  <img src="assets/OneAI-main.png" alt="macOS App — 默认对话主页" width="760">
</p>

<p align="center"><em>macOS App · 默认对话主页 —— 品牌入口 + 推荐话题，点话题即开聊。</em></p>

<p align="center">
  <img src="assets/oneai-tui-screenshot.jpg" alt="OneAI CLI — Plan 模式" width="860">
</p>

<p align="center"><em>交互式 CLI（<code>oneai-cli</code>）· Plan 模式 —— 思考气泡、计划面板、工具调用、审批。</em></p>

---

## Highlights

- **多类官方前端** —— macOS / Windows App、VS Code 扩展、浏览器扩展、CLI/TUI、移动 App，一套引擎喂全端，各端用各端的原生 UI。
- **统一引擎总线** —— 一套 `Directive`（前端→引擎）+ `EngineYield`（引擎→前端）协议，所有前端共用。
- **动态 AgentLoop** —— 每轮模型动态决策（直接回答 / 工具调用 / 委托子 Agent / 切换范式），迭代上限由 Token 预算约束。
- **DomainPack 一行切领域** —— 7 层声明式配置，可合并、可校验、可共享。
- **场景化 GroupChat** —— 引擎级多角色对话原语，5 个内置预设开箱即用。
- **生产级基础设施** —— ProviderPool 降级链 + SmartRouter 路由 + 限流 / 熔断 / 429 重试。
- **跨会话续接** —— 任务进度以事件日志落盘，新 session 自动 surface 上次未完成工作。

---

## 官方前端

同一套引擎，按你的场景挑一个用。

| 前端 | 状态 | 适合 |
|---|---|---|
| CLI / TUI | ✅ 全平台 | 通用 Agentic 编程 |
| macOS App | ✅ | 场景化多 Agent 对话 |
| Windows App | ⚠️ 源码构建 | 场景化多 Agent 对话 |
| VS Code 扩展 | ✅ | 在编辑器里聊 |
| 浏览器扩展 | ✅ macOS/Linux | 在浏览器里聊 |
| Android App | ✅ 源码构建 | 移动端场景对话 |
| iOS App | 🚧 在建 | 移动端 |
| HarmonyOS App | 🚧 在建 | 移动端 |

### 1. CLI / TUI

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

### 2. macOS App

原生 SwiftUI，不需要装 Xcode，Command Line Tools 即可编译。

```bash
./scripts/build_apple.sh
./platforms/macos/build_macos.sh
open platforms/macos/build/OneAI.app
```

打开 App → 设置面板配 Provider（类型 / API Key / Base URL / Model，Ollama 留空 key）→ 侧边栏选场景预设（面试演练 / 语言伙伴 / 辩论赛 / 写作工坊 / 头脑风暴）或自建 → 开聊。

### 3. Windows App

原生 WinUI 3 / C#，需 Visual Studio 含 WindowsAppSDK 1.8 workload。

```powershell
rustup target add x86_64-pc-windows-msvc
powershell ./scripts/build_windows.ps1
dotnet run --project platforms\windows\OneAI\OneAI.csproj -c Debug -r win-x64
```

`-r win-x64` 不可省。配置与 macOS 一致（设置面板配 Provider）。详见 [`platforms/windows/README.md`](platforms/windows/README.md)。

### 4. VS Code 扩展

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

### 5. 浏览器扩展

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

### 6. 移动端

Android App（Jetpack Compose / Kotlin）已跑通，`./scripts/build_android.sh` 跨 4 ABI 编译（需 `cargo-ndk` + Android Studio）。

iOS 与 HarmonyOS 是在建端口，分别需 Xcode / DevEco Studio，装上对应 IDE 重跑构建脚本即可。Linux 无独立原生 App，用 CLI。

---

## 贡献

欢迎贡献！先读 [CONTRIBUTING.md](CONTRIBUTING.md)——本地构建 / 测试命令、crate 分层规则、提 PR 前的自查清单。认领 [`good first issue`](https://github.com/Marssssss/OneAI/labels/good%20first%20issue) 上手；设计讨论走 [GitHub Discussions](https://github.com/Marssssss/OneAI/discussions)。行为准则见 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。

## 许可证

Apache-2.0 — 详情见 [LICENSE](LICENSE)。
