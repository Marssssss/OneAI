# OneAI 工具系统机制

> `Tool` trait + Registry + 执行器 + 15 内置工具 + MCP 客户端 + Footprint ladder，工具的「在哪一层落地」有决策规则。

## 职责

工具是 Agent 作用于世界的双手。工具系统负责注册、权限分级、安全执行、并按 Footprint ladder 决定一个新能力该以何种 schema 足迹暴露给模型（能用更小足迹就别放大）。

## Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn risk_level(&self) -> RiskLevel;
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}
pub trait PermissionAwareTool: Tool { fn permission_level(&self) -> PermissionLevel; }
```

## Footprint ladder

新能力的落地优先选足迹最小的那一档， climb only when lower rung can't satisfy：

```
extend (复用现有工具，无新 schema)
  → skill (一段 markdown 提示，零工具 schema)
  → service-gated tool (服务缺失时从 schema 消失，零足迹)
  → plugin / MCP tool (外部进程，条件连接)
  → core tool (常驻 schema)
```

`service_available()` 返回 `false` 的工具会从 schema **消失**（不是「禁用」），避免模型去试一个坏选项。`ToolRegistry::register_gated` / `build_tool_definitions_for_paradigm` 每轮应用此过滤。

## 关键类型与文件

| 项 | 位置 |
|---|---|
| `ToolRegistry` / `ToolExecutor` | `crates/oneai-tool/src/registry.rs`、`executor.rs` |
| 15 内置工具（Shell/FileRead/Edit/Write/ApplyPatch/List/Delete/Grep/Glob/Env/Notebook/Calculator/WebFetch/WebSearch/Browser） | `crates/oneai-tool/src/local_tools.rs` |
| 多文件统一 diff | `crates/oneai-tool/src/apply_patch.rs` |
| `FileOperations` trait + Local/Remote 实现 | `crates/oneai-tool/src/file_ops.rs` |
| ShellTool 安全黑名单 + 沙箱 | `crates/oneai-tool/src/sandbox.rs` |
| MCP 客户端（`rmcp`，stdio/SSE/streamable-http） | `crates/oneai-tool/src/mcp_real.rs` |

## 深入阅读

- [CLAUDE.md — Tools / Footprint ladder 章节](../CLAUDE.md)
- TerminalBackend（ShellTool 的执行后端）见 [Phase3.3](../crates/oneai-tool/src/sandbox.rs) 与 `crates/oneai-tool/src/file_ops.rs`
