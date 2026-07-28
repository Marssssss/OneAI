# Security Policy

OneAI executes LLM-driven tool calls (including a `ShellTool`), loads WASM
sandboxes, talks to LLM provider APIs, and runs MCP/A2A transports over the
network. Security-relevant bugs are taken seriously.

## Supported versions

Only the latest release line receives security fixes. See
[CHANGELOG.md](./CHANGELOG.md) for the current version.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Instead, use GitHub's private vulnerability reporting:

1. Go to the **Security** tab of this repository.
2. Click **Report a vulnerability**.
3. Describe the issue, the affected crate(s) (`crates/*`), and a minimal
   reproduction if possible.

You should receive an acknowledgement within 72 hours. If you have a proposed
fix, mention it in the report — please don't open a public PR for an unreported
vulnerability.

## Scope

In scope:

- Sandbox escapes in `ShellTool` (the blacklist / sandbox bypass).
- WASM sandbox breakout (`oneai-wasm` / wastime capability leakage).
- Permission-bypass paths: `deny_by_default` / `permission_overrides` /
  `auto_approve` / `require_confirmation` resolution being circumvented.
- Injection vectors in the 3-layer output parser (`oneai-parser`) that allow a
  crafted model response to escalate tool permissions.
- Credentials leakage (API keys ending up in traces, logs, or persisted state).

Out of scope:

- Bugs in third-party LLM provider APIs themselves.
- Self-inflicted issues from running untrusted tools outside the documented
  permission model.

## Disclosure

Once a fix is released, we publish a GitHub Security Advisory crediting the
reporter (unless they prefer to remain anonymous).
