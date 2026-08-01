# OneAI Tool System Mechanism

> `Tool` trait + Registry + executor + 15 built-in tools + MCP client + Footprint ladder — a decision rule for "which rung does a new capability live at".

## Responsibility

Tools are the agent's hands on the world. The tool system registers, grades permissions, executes safely, and uses the Footprint ladder to decide what schema footprint a new capability exposes to the model (smallest footprint that works).

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

A new capability prefers the rung with the smallest per-session schema footprint the model sees, climbing only when the lower rung can't satisfy it:

```
extend (compose existing tools, no new schema)
  → skill (a markdown prompt, zero tool schema)
  → service-gated tool (vanishes from schema when its service is missing, zero footprint)
  → plugin / MCP tool (external process, conditionally connected)
  → core tool (always present in schema)
```

A tool whose `service_available()` returns `false` **disappears from the schema** (not merely "disabled"), so the model never tries a broken option. `ToolRegistry::register_gated` / `build_tool_definitions_for_paradigm` apply this filter every iteration.

## Key types & files

| Item | Location |
|---|---|
| `ToolRegistry` / `ToolExecutor` | `crates/oneai-tool/src/registry.rs`, `executor.rs` |
| 15 built-in tools (Shell/FileRead/Edit/Write/ApplyPatch/List/Delete/Grep/Glob/Env/Notebook/Calculator/WebFetch/WebSearch/Browser) | `crates/oneai-tool/src/local_tools.rs` |
| Multi-file unified diff | `crates/oneai-tool/src/apply_patch.rs` |
| `FileOperations` trait + Local/Remote impls | `crates/oneai-tool/src/file_ops.rs` |
| ShellTool safety blacklist + sandbox | `crates/oneai-tool/src/sandbox.rs` |
| MCP client (`rmcp`, stdio/SSE/streamable-http) | `crates/oneai-tool/src/mcp_real.rs` |

## Further reading

- [CLAUDE.md — Tools / Footprint ladder](../CLAUDE.md)
- TerminalBackend (the ShellTool execution backend) — see `crates/oneai-tool/src/file_ops.rs` and `crates/oneai-tool/src/sandbox.rs`
