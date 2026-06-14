# OneAI TUI 重设计方案

## Context

OneAI 当前的 TUI（`examples/cli/src/tui.rs`）极其简陋：
- 聊天区域只是扁平文本 + emoji 前缀，无 markdown 渲染
- 侧栏是静态的工具名列表 + session ID，无交互性
- 输入区仅支持单行文本，无多行/vim 模式
- 无审批/权限 UI（始终使用 AutoApprovalGate）
- 无 diff 可视化、无折叠展开、无 cost/token 跟踪
- 无范式切换可视化、无迭代进度展示

本次重设计的目标：**融合 Claude Code 和 OpenCode 的 TUI 优点，摒弃其交互缺陷**，打造一个专业、高效、美观的终端 AI Agent 界面。

---

## 一、整体布局设计

采用 **品牌标题行 + 侧栏 + 主面板** 布局，侧栏可折叠（Tab 切换）：

顶部品牌标识 **独占1行居中**，彩色立体渐变效果，灵感来自 Claude Code 的品牌渲染：

```
┌─────────────────────────────────────────────────────────────────────┐
│                  ██████  ███   ███  ██   ██████                     │ ← 品牌行 (1行, 居中, 彩色立体)
│                  ██   ██ ████  ████ ██   ██   ██                    │    "OneAI" 用 ANSI art 或
│                  ██████  ██ ██ ██ █ █████ ██████                     │    渐变色 Bold 大字体渲染
├──────────┬──────────────────────────────────────────────────────────┤
│ 阿里百炼  │  💬 聊天区域                                            │
│ ·qwen-plus│                                                          │
│ ·a3f2    │  ┌─ User ────────────────────────────────────────────┐ │
│ ·ReAct#3 │  │ 分析一下这个 Rust 项目的性能瓶颈                       │ │
│ ·1.2k tok│  └──────────────────────────────────────────────────┘ │
│ ·$0.003  │                                                          │
│──────────│  ┌─ 🤖 Assistant ────────────────────────────────────┐ │
│ 📋 侧栏  │  │ 我先搜索一下项目中的关键文件...                       │ │
│ ┌ 会话 ─┐│  │                                                      │ │
│ │ ● 当前││  │  ┌─ 🔧 Tool: grep ──────────────────────────────┐ │ │
│ │ ○ 备用││  │  │ 搜索: "*.rs" 在 src/ 目录                       │ │ │
│ └───────┘│  │  │ ─── 展开/折叠 ────────────────────────────── │ │ │
│ ┌ 工具 ─┐│  │  │ 找到 23 个匹配文件                                │ │ │
│ │ 🔍grep││  │  └────────────────────────────────────────────────┘ │ │
│ │ ✏️edit ││  │                                                      │ │
│ │ 📂glob ││  │  根据搜索结果，以下文件可能是性能瓶颈:               │ │
│ │ 🧮calc ││  │                                                      │ │
│ └───────┘│  │  ┌─ rust ─────────────────────────────────────────┐ │ │
│ ┌ 范式 ─┐│  │  │ fn process_batch(items: &[Data]) {              │ │ │
│ │ ▸ReAct ││  │  │     // ...                                      │ │ │
│ │ ▸Plan  ││  │  │ }                                               │ │ │
│ │ ▸Reflct││  │  └────────────────────────────────────────────────┘ │ │
│ └───────┘│  └──────────────────────────────────────────────────┘ │
│ ┌ 费用 ─┐│                                                          │
│ │ $0.003 ││  ── ⏳ thinking... ────────────────────────────────     │
│ │ 📊2.4k ││                                                          │
│ └───────┘│                                                          │
│          ├──────────────────────────────────────────────────────────┤
│          │  oneai> _                                                │ ← 输入区 (3行)
│          │  [Enter=发送 Esc=多行 Tab=侧栏 ↑↓=历史]                  │
└──────────┴──────────────────────────────────────────────────────────┘
```

### 品牌行 (Brand Line) — 1 行, 居中

**设计灵感**: Claude Code 启动时在终端顶部显示品牌标识，带有彩色渐变效果。

**OneAI 品牌渲染方案**:

采用 **ANSI 真彩色渐变文字** 方式渲染 "OneAI" 品牌名：

```
                  O n e A I
```

每个字符用不同颜色渲染，形成从左到右的渐变立体效果：

| 字符 | 颜色 | 说明 |
|------|------|------|
| `O` | #FF6B6B (珊瑚红) | 渐变起点 |
| `n` | #4ECDC4 (青绿) | 中段 |
| `e` | #45B7D1 (天蓝) | 中段 |
| `A` | #96CEB4 (薄荷绿) | 中段 |
| `I` | #FFEAA7 (暖金) | 渐变终点 |

渲染时每个字符使用 `Style::default().fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD)`，
加上字符间的微间距，形成立体渐变视觉。

**备选方案**: 在终端宽度 ≥ 100 时，用 ANSI block art 渲染更大的品牌标识：

```
              ██████  ███   ███  ██   ██████
              ██   ██ ████  ████ ██   ██   ██
              ██████  ██ ██ ██ █ █████ ██████
```

每个 █ 块用渐变色填充（同行从左到右渐变），终端宽度 < 100 时退回为单行文字。

**思考状态时**: 品牌行右侧追加 `⏳ thinking...` + spinner 动画。

---

**侧栏顶部区域** (侧栏上半部分，替代原状态栏功能):

侧栏上方显示上下文信息（原状态栏内容迁移到侧栏），避免品牌行被信息打断：

| 区域 | 内容 | 样式 |
|------|------|------|
| 第一行 | Provider · Model | Cyan |
| 第二行 | Session ID 前 8 位 | DarkGray |
| 第三行 | 当前范式 + 迭代号 | Yellow |
| 第四行 | Token 计数 + 费用 | Green |

折叠侧栏后，这些信息以紧凑形式显示在品牌行下方（作为副标题行）：

```
┌─────────────────────────────────────────────────────────────────────┐
│                  O n e A I                                          │ ← 品牌行
│                  里百炼·qwen-plus | a3f2 | ReAct#3 | 1.2k $0.003 │ ← 信息行 (折叠时)
├─────────────────────────────────────────────────────────────────────┤
│ 💬 聊天区域 (全宽)                                                  │
│  ...                                                                │
├─────────────────────────────────────────────────────────────────────┤
│ oneai> _                                                            │
│ [Enter=发送 Esc=多行 Tab=侧栏 ↑↓=历史]                              │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 二、审批/权限提示 UI

当高权限工具(Standard/Full)需要审批时，聊天区域中断显示审批卡片：

```
│  ┌─ ⚠️ Approval Required ──────────────────────────────────────┐ │
│  │ Tool: shell (Full permission)                                │ │
│  │ Command: rm -rf /tmp/build_cache                             │ │
│  │                                                              │ │
│  │ [Y] Approve  [N] Deny  [M] Modify  [A] Always for session   │ │
│  │                                                              │ │
│  │ > _                                                          │ │
│  └──────────────────────────────────────────────────────────────┘ │
```

信任级别选项：
- **Y (Approve)**: 本次允许
- **N (Deny)**: 本次拒绝
- **M (Modify)**: 修改参数后允许
- **A (Always)**: 本会话中该工具自动放行（存入 session-level allowlist）

---

## 三、组件详细设计

### 3.1 品牌行 (Brand Line) — 1 行, 居中, 形式独立

**渲染方式**: "OneAI" 文字居中，每个字母用 Rgb 渐变色 + Bold 渲染：

| 字符 | RGB 颜色 | 效果 |
|------|----------|------|
| `O` | #FF6B6B (珊瑚红) | 渐变起点 |
| `n` | #4ECDC4 (青绿) | |
| `e` | #45B7D1 (天蓝) | |
| `A` | #96CEB4 (薄荷绿) | |
| `I` | #FFEAA7 (暖金) | 渐变终点 |

- 所有字符均使用 `Modifier::BOLD` + `Modifier::RAPID_BLINK` (微闪烁，仅思考时)
- 字符间使用半角空格 `"O n e A I"` 形成视觉间距
- 行背景使用 `Color::Rgb(30, 30, 46)` (深蓝灰) 突出品牌行

**宽度 ≥ 100 时**: 渲染 ANSI block art 大字版 "OneAI" (3行高)，每行 █ 填充用同行渐变色。
**宽度 < 100 时**: 退回单行文字版。

**思考状态**: 品牌行右侧追加 `⏳ thinking...` + spinner 动画，使用 DarkGray 色。

**侧栏展开时**: 上下文信息（Provider、Session、范式、费用）在侧栏顶部区域显示。
**侧栏折叠时**: 上下文信息作为副标题行显示在品牌行下方，紧凑格式：
`阿里百炼·qwen-plus | a3f2 | ReAct#3 | 1.2k $0.003`

**render/brand.rs 实现要点**:
```rust
fn draw_brand(f: &mut Frame, rect: Rect, app: &App) {
    let brand_chars = vec![
        ('O', Color::Rgb(255, 107, 107)),
        ('n', Color::Rgb(78, 205, 196)),
        ('e', Color::Rgb(69, 183, 209)),
        ('A', Color::Rgb(150, 206, 180)),
        ('I', Color::Rgb(255, 234, 167)),
    ];
    // 计算居中偏移
    // 渲染每个字符为 Styled Span
    // 思考时追加 spinner
}
```

### 3.2 侧栏 (Sidebar) — 24 列宽，可折叠

**顶部上下文区 (Context)**（替代原独立状态栏）:
- Provider · Model (如 `阿里百炼 · qwen-plus`) — Cyan
- Session ID 前 8 位 — DarkGray
- 当前范式 + 迭代号 (如 `ReAct #3`) — Yellow
- Token 计数 + 费用 (如 `1.2k $0.003`) — Green
- 思考时: `⏳ thinking...` — DarkGray + spinner

**四个分区**，每个有标题行：

**会话区 (Sessions)**:
- 当前 session ID (高亮)
- 备选 session (暗色，未来支持多会话切换)

**工具区 (Tools)**:
- 每个工具一行，显示 emoji + 名称
- 当前被调用的工具闪烁/高亮
- 权限等级标签 (R/S/F)

**范式区 (Paradigm)**:
- 四种范式列表
- 当前激活范式高亮 + ▸ 标记
- 不可激活范式灰色

**费用区 (Cost)**:
- 累计费用 ($)
- Token 使用量 (prompt + completion)
- 剩余预算进度条

### 3.3 聊天区域 (Chat Area) — 可滚动 Viewport

消息类型及渲染方式：

| 类型 | 样式 | 说明 |
|------|------|------|
| **User** | 蓝色气泡框，左侧对齐 | 用户消息 |
| **Assistant** | 绿色气泡框，左侧对齐，内含 markdown 渲染 | AI 回复 |
| **Tool Call** | 紫色卡片框，可折叠 | 工具调用请求 |
| **Tool Result** | 蓝/绿色卡片框（成功绿/失败红），可折叠 | 工具结果 |
| **Approval** | 黄色警告卡片框 | 审批提示 |
| **System** | 灰色，无气泡 | 范式切换、checkpoint、错误等 |
| **Thinking** | 灰色 + spinner | Agent 正在思考 |

**Markdown 渲染**（基于 `syntect` crate 实现完整语法高亮）：
- 代码块：`` ``` `` 包围，语言标注，syntect 语法高亮渲染到 ANSI 颜色
- 列表：缩进 + 前缀符号
- 标题：Bold + 大字号前缀
- 行内代码：反引号包围，不同背景色
- 链接：下划线 + 蓝色
- Bold/Italic：ratatui Modifier::BOLD / Modifier::ITALIC

**syntect 渲染流程**：
1. 解析 markdown 文本，识别代码块 + 语言标签
2. 对每个代码块调用 `syntect::html::highlighted_snippet_for_string` 获取 Token 序列
3. 将 syntect 的 Theme 映射到 ratatui Style（RGB → ANSI 256 或 true color）
4. 代码块外围用圆角边框，标题行显示语言名
5. 非 code-block 文本做简单的 markdown-to-ratatui 转换（regex 匹配 bold/italic/links）

**新增依赖**: `syntect` (workspace Cargo.toml)

**折叠/展开**：
- Tool Call 卡片默认折叠（只显示工具名 + 参数摘要）
- 按 Enter 或点击可展开显示完整参数 + 结果
- Tool Result 超过 200 字符时默认折叠
- Diff 结果默认折叠为摘要行

### 3.4 输入区 (Input Area) — 3 行

**两种模式**：
1. **单行模式 (默认)**: 简洁输入，Enter 发送
2. **多行编辑模式 (Esc 切换)**: vim 风格编辑器

**单行模式**：
```
oneai> 在这里输入消息_
[Enter=发送 Esc=多行 Tab=侧栏 ↑↓=历史]
```

**多行编辑模式** (Esc 进入):
```
┌─ 输入 (多行 vim 模式) ─────────────────────────────────────────┐
│ 在这里输入更长的消息，                                            │
│ 支持多行编辑...                                                  │
│ _                                                                │
└──────────────────────────────────────────────────────────────────┘
[Enter=发送 Esc=退出多行 Ctrl+C=取消 j/k=移动 i=插入]
```

**Slash 命令** (在两种模式中均可使用):
| 命令 | 说明 |
|------|------|
| `/help`, `/h` | 显示帮助信息 |
| `/clear` | 清空对话 |
| `/compact` | 压缩对话上下文 |
| `/cost` | 显示费用明细 |
| `/tools`, `/t` | 列出注册工具 |
| `/tool <name> <json>` | 直接调用工具 |
| `/session` | 显示会话信息 |
| `/paradigm <name>` | 切换范式 |
| `/quit`, `/q` | 退出 |

---

## 四、配色方案

基于 ANSI 256 色，确保在绝大多数终端中良好显示：

| 用途 | 颜色 | ANSI Code |
|------|------|-----------|
| 品牌标识 | Bold White | 15 |
| Provider/Model | Cyan | 12 |
| User 消息 | Bright Cyan | 14 |
| Assistant 消息 | Bright Green | 10 |
| Tool Call | Bright Magenta | 13 |
| Tool Result (成功) | Blue | 9 |
| Tool Result (失败) | Bright Red | 9 |
| Approval | Bright Yellow | 11 |
| System/Thinking | Dark Gray | 8 |
| 错误 | Bright Red | 9 |
| 气泡框边框 | Gray | 7 |
| 代码块背景 | Dark Gray (238) | 238 |
| 代码块关键字 | Bright Yellow (184) | 184 |
| Diff 新增行 | Green BG (22) | 22 |
| Diff 删除行 | Red BG (52) | 52 |
| 进度条填充 | Bright Green | 10 |
| 进度条背景 | Dark Gray | 8 |
| 状态栏背景 | Dark Gray (236) | 236 |

---

## 五、快捷键映射

| 快捷键 | 功能 | 备注 |
|--------|------|------|
| `Enter` | 发送消息 | 单行模式 |
| `Esc` | 切换多行模式 / 退出多行模式 | 双功能 |
| `Ctrl+Enter` | 在单行模式插入换行 | 多行模式中 Enter 直接发送 |
| `Tab` | 切换侧栏显示 | |
| `Ctrl+C` | 取消当前操作 / 强制退出 | 双功能（有内容时取消，无内容时退出） |
| `↑ / ↓` | 消息历史导航 | 输入为空时 |
| `Ctrl+↑ / Ctrl+↓` | 聊天区域滚动 | |
| `Enter` (在折叠卡片上) | 展开/折叠 | 聊天区域中 |
| `/` | 进入命令模式 | 输入开头 |
| `Ctrl+L` | 清屏 | |
| `Ctrl+Z` | 撤销输入 | |

多行 vim 模式额外快捷键：
| 快捷键 | 功能 |
|--------|------|
| `i` | 进入插入模式 |
| `Esc` | 返回普通模式 |
| `h/j/k/l` | 移动光标 |
| `x` | 删除字符 |
| `dd` | 删除行 |
| `0` | 行首 |
| `$` | 行尾 |

---

## 六、解决 Claude Code 的已知 UX 问题

| Claude Code 问题 | OneAI 解决方案 |
|------------------|---------------|
| **长输出慢滚动** | Viewport 虚拟化渲染 — 只渲染可见行，不渲染全量文本 |
| **输入延迟** | 输入区渲染独立于 stream 处理 — 解耦事件循环 |
| **多行输入不便** | vim 风格多行编辑器 + Ctrl+Enter 单行换行 |
| **历史导航困难** | ↑↓ 快捷键浏览消息历史 + Ctrl+↑↓ 滚动聊天 |
| **diff 渲染慢** | diff 默认折叠，展开时增量渲染 |
| **streaming 渲染延迟** | 使用 ratatui viewport + 50ms poll interval + 异步 observer channel |

---

## 七、后端修改清单

### 7.1 AgentLoopObserver 增强 (`crates/oneai-agent/src/agent_loop.rs`)

当前 observer 缺少以下事件：

1. **新增 `on_approval_request`**: 通知 UI 有审批请求
2. **新增 `on_approval_response`**: 通知 UI 审批结果
3. **新增 `on_token_usage`**: 每次 inference 后通知 token 使用量
4. **新增 `on_cost_update`**: 通知费用变化
5. **修改 `on_tool_calls`**: 增加 `ToolIntentDetected` 事件（已有 streaming.rs 中的 StreamEvent）
6. **增强 `on_stream_chunk`**: 增加流式工具意图通知

```rust
pub trait AgentLoopObserver: Send + Sync {
    // 已有方法保持不变...

    /// Called when an approval request is pending (high-risk tool).
    fn on_approval_request(&self, request: &ApprovalRequest) {}

    /// Called when the user responds to an approval request.
    fn on_approval_response(&self, response: &ApprovalResponse) {}

    /// Called after each inference with token usage stats.
    fn on_token_usage(&self, usage: &TokenUsage) {}

    /// Called when cost updates (cumulative session cost).
    fn on_cost_update(&self, cost: f64, currency: &str) {}
}
```

### 7.2 审批门接入 TUI (`examples/cli/src/tui.rs` + `crates/oneai-tool/src/approval.rs`)

当前问题：TUI 使用 `AutoApprovalGate`（自动审批），`ChannelApprovalGateWithThreshold` 已实现但未接入。

修改：
1. 将 `AutoApprovalGate` 替换为 `ChannelApprovalGateWithThreshold`
2. threshold 设为 `RiskLevel::Medium`（Low 自动放行，Medium+ 走通道）
3. TUI 主循环中增加审批事件处理：从 `approval_rx` 接收 `ApprovalPendingItem`
4. 审批时切换 UI 状态为 "approval mode"，显示审批卡片
5. 用户按键（Y/N/M/A）→ 构造 `ApprovalResponse` → 通过 `response_tx` 发回
6. "Always" 选项 → 维护 session-level allowlist，后续同工具自动放行

### 7.3 TokenUsage/Cost 跟踪 (`examples/cli/src/tui.rs`)

1. 在 `ObserverEvent` 中增加 `TokenUsage(TokenUsage)` 和 `CostUpdate(f64)` 事件
2. `App` 结构增加 `token_usage: TokenUsage` 和 `session_cost: f64` 字段
3. 在 `TuiObserver::on_token_usage` 和 `on_cost_update` 中发送这些事件
4. 状态栏渲染使用这些字段

### 7.4 AppSession.run_agent 增强 (`crates/oneai-app/src/session.rs`)

`run_agent` 当前不传递审批事件和 token 使用到 observer。需要：
1. 在 `AgentLoop` 内部，每次 inference 后调用 `observer.on_token_usage(&response.usage)`
2. 在审批流程中，调用 `observer.on_approval_request` 和 `observer.on_approval_response`
3. 累计费用计算：根据 provider 的 pricing model 算出 session cost

### 7.5 AgentLoopConfig 增加 streaming 通知 (`crates/oneai-agent/src/agent_loop.rs`)

1. `AgentLoopConfig` 增加 `notify_approval_events: bool`（默认 true）
2. `AgentLoopConfig` 增加 `notify_token_usage: bool`（默认 true）
3. 在 `run_loop` 中根据配置决定是否通知

---

## 八、TUI 模块重构 (`examples/cli/src/tui.rs`)

当前 `tui.rs` 是一个 778 行的单文件，需要拆分为模块化结构：

```
examples/cli/src/
├── main.rs            # 入口，环境变量解析，启动 TUI
└── tui/
    ├── mod.rs          # 公共导出，run_tui() 主函数
    ├── app.rs          # App 状态结构 + 事件处理
    ├── observer.rs     # TuiObserver + ObserverEvent 定义
    ├── render/
    │   ├── mod.rs      # draw() 主函数，布局编排
    │   ├── brand.rs        # 品牌行渲染（彩色渐变 + ANSI art 大字版）
    │   ├── context_bar.rs  # 侧栏折叠时的副标题上下文行
    │   ├── sidebar.rs      # 侧栏渲染（含顶部上下文区 + 四分区）
    │   ├── chat.rs         # 聊天区域渲染 + Viewport 管理
    │   ├── input.rs        # 输入区渲染（单行 + 多行模式）
    │   ├── message.rs      # 消息类型渲染（User/Assistant/Tool/System）
    │   ├── approval.rs     # 审批卡片渲染
    │   ├── markdown.rs     # Markdown 渲染（代码块、列表、标题）
    │   └── diff.rs         # Diff 渲染（+/- 行标记）
    │   └── spinner.rs      # 动画 spinner
    ├── theme.rs        # 颜色方案定义
    ├── input_mode.rs   # InputMode enum (SingleLine / MultiLineVim)
    ├── session.rs      # SessionState + allowlist 管理
    └── history.rs      # 消息历史导航
```

---

## 九、关键数据结构变更

### 9.1 ChatMessage 增强

```rust
pub struct ChatMessage {
    pub id: String,            // 新增：消息唯一 ID（用于折叠状态管理）
    pub role: ChatRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,  // 新增：消息时间戳
    pub collapsed: bool,       // 新增：是否折叠
    pub token_usage: Option<TokenUsage>,  // 新增：该轮 token 使用
    pub paradigm: Option<ParadigmKind>,   // 新增：该轮范式
    pub iteration: Option<usize>,         // 新增：该轮迭代号
}

pub enum ChatRole {
    User,
    Assistant,
    System,
    ToolCall,        // 增加字段：call_id, tool_name, args
    ToolResult,      // 增加字段：call_id, success
    Iteration,
    Error,
    Approval,        // 新增
    Thinking,        // 新增
}
```

### 9.2 App 状态增强

```rust
pub struct App {
    // 已有字段保持不变...
    pub should_quit: bool,
    pub show_sidebar: bool,
    pub input: String,
    pub messages: Vec<ChatMessage>,
    pub scroll_offset: u16,
    pub scrollbar_state: ScrollbarState,
    pub tool_names: Vec<String>,
    pub provider_info: String,
    pub session_id: String,
    pub is_thinking: bool,

    // 新增字段
    pub input_mode: InputMode,          // 单行/多行 vim
    pub active_paradigm: ParadigmKind,  // 当前范式
    pub current_iteration: usize,       // 当前迭代号
    pub token_usage: TokenUsage,        // 累计 token 使用
    pub session_cost: f64,              // 累计费用
    pub collapsed_ids: HashSet<String>, // 折叠的消息 ID 集合
    pub message_history: Vec<String>,   // 消息历史（↑↓ 导航）
    pub history_index: usize,           // 历史导航当前位置
    pub approval_pending: Option<ApprovalPendingItem>, // 当前待审批项
    pub session_allowlist: HashSet<String>, // session-level 审批放行列表
    pub vim_mode: VimMode,              // Normal / Insert（多行模式时）
    pub spinner_frame: usize,           // spinner 动画帧号
}
```

### 9.3 InputMode

```rust
pub enum InputMode {
    SingleLine,
    MultiLineVim { cursor_position: usize, mode: VimMode },
}

pub enum VimMode {
    Normal,
    Insert,
}
```

---

## 十、新增依赖

在 `Cargo.toml` (workspace) 和 `examples/cli/Cargo.toml` 中添加：

| Crate | 版本 | 用途 |
|-------|------|------|
| `syntect` | 5.2 | 代码块语法高亮（Rust 内置语法定义，无需外部文件） |
| `pulldown-cmark` | 0.11 | Markdown 解析（将 markdown 文本解析为 AST 事件流） |
| `unicode-width` | 0.2 | 精确计算 Unicode 字符宽度（中文字符等） |

注意：syntect 使用 `default-on` feature 即可（包含常用语法定义），不需要 `extra-syntaxes`。

---

## 十一、实现优先级和阶段划分

### Phase 1: 基础框架重构 (优先级最高)

1. 拆分 `tui.rs` 为模块化结构
2. 实现新的 App 状态结构（增加所有新字段）
3. 实现 Viewport 虚拟化渲染（解决慢滚动问题）
4. 重写 draw() 主函数，按新布局渲染
5. 实现状态栏渲染

**验证**: 编译通过，基础布局可显示，viewport 滚动流畅

### Phase 2: 气泡式消息渲染

1. 实现 ChatMessage 新结构（id, collapsed, timestamp 等）
2. 实现 message.rs — User/Assistant 气泡框渲染
3. 实现 markdown.rs — pulldown-cmark 解析 + syntect 语法高亮
4. 实现 Tool Call / Tool Result 卡片渲染 + 折叠展开
5. 实现 Thinking spinner 动画

**验证**: 消息以气泡框形式显示，代码块语法高亮完整，工具调用可折叠

### Phase 3: 输入模式增强

1. 实现 InputMode enum 和 vim 模式
2. 实现单行 → 多行模式切换 (Esc)
3. 实现 vim 模式键盘处理 (i/h/j/k/l/x/dd)
4. 实现消息历史导航 (↑↓)
5. 实现 slash 命令增强 (/cost, /compact, /session)

**验证**: 多行 vim 编辑器可用，历史导航可用，slash 命令工作

### Phase 4: 审批 UI 接入

1. 替换 AutoApprovalGate → ChannelApprovalGateWithThreshold
2. 实现 approval.rs — 审批卡片渲染
3. TUI 主循环处理 ApprovalPendingItem
4. 实现 Y/N/M/A 按键响应 → ApprovalResponse
5. 实现 session-level allowlist

**验证**: 高风险工具触发审批卡片，按键响应正确，Always 选项生效

### Phase 5: 侧栏增强 + Token/Cost 跟踪

1. 实现侧栏四个分区渲染
2. 增强 AgentLoopObserver (on_token_usage, on_cost_update, on_approval_request)
3. 实现 token/cost 数据流到 TUI
4. 状态栏显示 token/cost 信息
5. 侧栏工具高亮（当前被调用工具）

**验证**: 侧栏显示完整信息，状态栏实时更新 token/cost，范式指示器工作

### Phase 6: Diff 渲染 + 高级功能

1. 实现 diff.rs — diff 可视化（+/- 行标记）
2. 实现文件操作工具的 diff 输出格式
3. 实现搜索/过滤功能（Ctrl+F 搜索消息）
4. 实现多会话管理（侧栏会话切换）
5. 实现 /compact 命令后端接入

**验证**: diff 输出可视化正确，搜索功能可用

---

## 十二、验证方案

1. **编译验证**: `cargo build -p oneai-cli-demo` 成功
2. **测试验证**: 现有 212 测试全通过
3. **手动验证**: 运行 TUI，逐项检查：
   - 布局渲染正确（侧栏 + 状态栏 + 聊天区 + 输入区）
   - 消息气泡框显示
   - Markdown 代码块高亮
   - 工具调用折叠展开
   - 审批卡片 Y/N/M/A 响应
   - 多行 vim 模式切换和编辑
   - 消息历史 ↑↓ 导航
   - Token/cost 实时更新
   - 侧栏四个分区显示
   - Diff 渲染
4. **性能验证**: 长输出(1000+ 行)滚动流畅，无延迟
5. **审批测试**: 高风险工具触发审批，低风险工具自动放行