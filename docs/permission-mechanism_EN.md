# OneAI Permission & InteractionGate Mechanism

> Three-tier permissions + a unified 5-decision-point gate that collapses "should the model be allowed to do this" into one swappable trait.

## Responsibility

Tools are neither fully open nor blanket-approved. The permission model grades by risk (Read auto / Standard policy-dependent / Full requires approval), and `InteractionGate` guards 5 key decision points uniformly — wireable to no-op, a channel-bridged UI, threshold passthrough, deny-all, or a native dialog.

## Three-tier permissions

`Read` (auto-approved) / `Standard` (policy-dependent) / `Full` (must approve). Runtime resolution order:

```
deny_by_default → permission_overrides → auto_approve → require_confirmation → tool.risk_level()
```

The `PermissionResolver` trait (in `oneai-core`, to avoid dependency-direction issues) unifies the three resolution paths.

## 5 decision points

`InteractionGate` guards:

| Point | Purpose |
|---|---|
| `PreInfer` | Rewrite / skip before inference |
| `PostInfer` | Validate / replace after inference |
| `ToolApproval` | Release high-risk tools (native dialog) |
| `PlanDecision` | Planning tradeoff |
| `PlanReview` | Final plan accept / reject / Revise |

## Key types & files

| Item | Location |
|---|---|
| `PermissionLevel` / `InteractionGate` trait / `PermissionResolver` | `crates/oneai-core/src/traits.rs`, `crates/oneai-core/src/types.rs` |
| `Noop` / `Channel` / `Threshold` / `DenyAll` gate | `crates/oneai-tool/src/interaction_gate.rs` |
| Native `PlatformInteractionGate` (NSAlert / MessageBox / AlertDialog / CommonDialog) | `crates/oneai-platform-{desktop,android,ios,harmony}/src/` |

## Built-in impls

- `NoopInteractionGate` — pass all (tests / SDK)
- `ChannelInteractionGate` — mpsc + oneshot bridge to UI thread, configurable per point
- `ThresholdInteractionGate` — low-risk passthrough, rest via channel
- `DenyAllInteractionGate` — deny all
- `PlatformInteractionGate` — platform-side native dialog for `ToolApproval`

## Further reading

- [CLAUDE.md — Permission model](../CLAUDE.md)
- Security hardening details in [evolution-plan §1.4](evolution-plan-2026-07.md) (Chinese)
