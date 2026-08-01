# OneAI 权限与 InteractionGate 机制

> 三级权限 + 统一 5 决策点 gate，把「该不该让模型做这件事」收敛到一个可替换的 trait。

## 职责

工具不是全放行、也不是一刀切审批。权限模型按风险分级（Read 自动 / Standard 视策略 / Full 必审批），由 `InteractionGate` 在 5 个关键决策点统一把守，可对接无放行、通道桥 UI、阈值放行、全拒、或原生对话框。

## 三级权限

`Read`（自动批准）/ `Standard`（视策略）/ `Full`（必审批）。运行时解析序：

```
deny_by_default → permission_overrides → auto_approve → require_confirmation → tool.risk_level()
```

`PermissionResolver` trait（放 `oneai-core`，绕依赖方向）统一三路径解析。

## 5 决策点

`InteractionGate` 把守：

| 决策点 | 作用 |
|---|---|
| `PreInfer` | 推理前改写 / 跳过 |
| `PostInfer` | 推理后校验 / 替换 |
| `ToolApproval` | 高风险工具放行（对接原生对话框） |
| `PlanDecision` | 规划权衡 |
| `PlanReview` | 最终计划 accept / reject / Revise |

## 关键类型与文件

| 项 | 位置 |
|---|---|
| `PermissionLevel` / `InteractionGate` trait / `PermissionResolver` | `crates/oneai-core/src/traits.rs`、`crates/oneai-core/src/types.rs` |
| `Noop` / `Channel` / `Threshold` / `DenyAll` gate | `crates/oneai-tool/src/interaction_gate.rs` |
| 原生 `PlatformInteractionGate`（NSAlert / MessageBox / AlertDialog / CommonDialog） | `crates/oneai-platform-{desktop,android,ios,harmony}/src/` |

## 内置实现

- `NoopInteractionGate` — 全放行（测试 / SDK）
- `ChannelInteractionGate` — mpsc + oneshot 桥 UI 线程，按决策点可配
- `ThresholdInteractionGate` — 低风险放行、余走通道
- `DenyAllInteractionGate` — 全拒
- `PlatformInteractionGate` — 平台侧原生对话框处理 `ToolApproval`

## 深入阅读

- [CLAUDE.md — 权限模型章节](../CLAUDE.md)
- 安全护栏细节见 [evolution-plan §1.4](evolution-plan-2026-07.md)
