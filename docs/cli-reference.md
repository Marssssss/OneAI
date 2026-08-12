# OneAI CLI 子命令参考

`oneai-cli`（bin `oneai`，不带子命令时默认进入 TUI）的子命令总览。全部定义在 `examples/cli/src/main.rs` 的 clap derive；运行 `oneai --help` 或 `oneai <sub> --help` 查任意子命令的完整参数。

> 对话内的斜杠命令（`/tools`、`/skills`、`/wf` 等）见 [README 快速上手 → 对话内斜杠命令](../README.md#二tui--cli通用-agentic-执行)。

## 会话与推理

```bash
oneai                                  # 启动交互式 TUI（默认）
oneai chat [--domain coding] [--model gpt-4o] [--user <id>]   # 启动 TUI（显式）
oneai run "<prompt>" [--domain coding] [--model ...] [--user <id>]  # 非交互单次推理，输出到 stdout
oneai version                          # 版本信息
oneai init [--format oneai|agents|claude] [--path <dir>] [--force] [--no-llm]  # 生成 ONEAI.md/AGENTS.md/CLAUDE.md
```

## DomainPack（领域配置包）

```bash
oneai pack show <name>                 # 查看 pack 详情
oneai pack install <path|git-url>      # 从本地路径或 git 安装
oneai pack validate spec.toml         # 对照 JSON Schema 校验（结构 + 语义）
oneai pack check <name>               # 对照规范检查已安装 pack
oneai pack containerized              # 启用容器化 CodingPack（VM/容器即边界，同名工具全接同一 backend）
```

机制：[domain-pack-mechanism.md](domain-pack-mechanism.md)。

## Skill 与 Curator（技能生命周期）

```bash
oneai skill show <name>                # 查看技能详情
oneai curator pin <name>               # 钉住（豁免自动退役）
oneai curator unpin <name>             # 解除钉住
oneai curator archive <name>           # 手动归档（可逆）
oneai curator restore <name>           # 恢复归档技能
oneai curator rollback <id>            # 从快照恢复技能 + 元数据
```

技能从 `.claude/.agents/.opencode/.oneai skills` 约定目录发现；Curator 永不删除只归档 + 可回滚。机制：[CLAUDE.md — Skill](../CLAUDE.md)。

## 评测框架

```bash
oneai eval run <suite> [--format markdown|json|compact] [--profile] [--record <path>]  # 运行套件（--profile 输出效率轴，--record 录制轨迹）
oneai eval score <suite>               # 仅跑指标（不执行 agent）
oneai eval replay <path>               # 幽灵重放录制轨迹，校验确定性
oneai eval swebench --dataset ./swe_bench_lite.jsonl [--instances <ids>] [--limit N] [--modal]  # SWE-bench 三轴评测
oneai eval memory --suite <jsonl> [--data <file>] [--metrics recall_at_k,f1,bleu1] [--no-embedding] [--k 5] [--format markdown]  # 记忆评测（LongMemEval 5 能力）
```

机制：[eval-mechanism.md](eval-mechanism.md)。

## 工作流与状态图

```bash
oneai workflow list [--domain coding]  # 列出 DAG 工作流 + 状态图
oneai workflow show <name>             # ASCII 渲染工作流 DAG + 步骤
oneai workflow run <name> [task] [--domain ...] [--model ...] [--user <id>]  # 端到端执行 DAG 工作流
oneai graph list [--domain coding]     # 列出状态图（react/plan/reflect/explore）
oneai graph show <name>                # ASCII 渲染状态图
oneai graph run <name> <task> [--domain ...] [--model ...] [--user <id>]   # 用真实 Provider 执行状态图
```

机制：[workflow-mechanism.md](workflow-mechanism.md)。

## 多 Agent 协作

主 Loop 内通过模型驱动的 `delegate` 元工具分层委托子 Agent（支持一轮多委托 + 依赖感知的并行波次调度），`switch_paradigm` 切换 Plan/Reflect/Explore 等固定图流；引擎级 GroupChat 原语驱动场景化多角色对话。无需独立的 Team/Swarm/Handoff 编排层——聚合/路由/辩论等模式由确定性 StateGraph 表达。机制：[multi-agent-mechanism.md](multi-agent-mechanism.md)。

## Provider 池与智能路由

```bash
oneai provider status                  # Provider 池状态：活跃 provider、健康、熔断
oneai provider fallback-log [--limit 20]  # 近期降级事件
oneai provider test                     # 连通性检查池中所有 provider
oneai provider route "任务描述" [--strategy balanced|cost|latency|quality]  # 路由决策 dry-run
oneai provider route-log [--limit 10]  # 近期路由决策及理由
oneai provider route-config             # 当前路由策略与配置
```

机制：[provider-mechanism.md](provider-mechanism.md)。

## Token 计数与上下文管理

```bash
oneai token count "文本" [--model ...]  # 统计 token 数
oneai token estimate [--model ...]      # 估算样例对话的 token 数
oneai token context <model>            # 查看模型上下文窗口画像
oneai token models                      # 列出已知 tokenizer 画像
oneai token fits "文本" --model <model> # 检查文本是否装得下上下文窗口
oneai token probe [--model ...]        # 探测 Provider 模型元数据端点（L2），展示三层解析结果
```

机制：[context-management-mechanism.md](context-management-mechanism.md)。

## 用量记录（纯 token 维度，无 USD）

```bash
oneai usage session <id>               # 单会话用量详情
oneai usage export [--format json|csv]  # 导出用量记录
```

机制：[persistence-mechanism.md](persistence-mechanism.md)。

## 记忆（跨会话持久事实）

```bash
oneai memory search <kw> [--user <id>] [--top_k 10]  # 关键词/语义检索持久事实
oneai memory list [--user <id>] [--session <id>]      # 列出某用户/会话的事实
```

机制：[memory-mechanism.md](memory-mechanism.md)。

## 持久化会话（SQLite）

```bash
oneai session list                     # 列出已保存会话
oneai session resume <id>             # 预览某会话对话历史（print-only；live 续接走 tasks continue）
oneai session delete <id>             # 删除会话
oneai session info <id>               # 查看会话详情
oneai session decay                    # 跑记忆衰减（按 salience 淘汰 → 归档）
oneai session export-hf <id>          # 导出为 OpenAI messages JSONL（含脱敏 + 可选 working-state 事件）
```

机制：[persistence-mechanism.md](persistence-mechanism.md)。

## 工作状态（跨 session 任务续接）

```bash
oneai tasks list                       # 列出未完成任务（读 index.json）
oneai tasks show <id>                  # 查看某任务的目标/步骤/决策/卡点
oneai tasks continue <id>             # 新 session 绑定该任务、derive 进内存、续接
oneai tasks archive <id>              # 归档任务（gzip 事件日志）
```

机制：[working-state-mechanism.md](working-state-mechanism.md)。

## Cron（定时调度）

```bash
oneai cron add --name <n> --schedule "30m|every 2h|ISO|0 9 * * *" --task "<prompt>" [--platform loopback] [--channel <ch>] [--session <id>] [--pack coding] [--deliver origin|silent]
oneai cron list                        # 列出定时任务
oneai cron rm <id>                     # 移除任务
oneai cron fire <id>                   # 手动触发（force，绕过 due 窗口但不重复）
oneai cron serve [--cron-bind 0.0.0.0:9091] [--gateway-bind 0.0.0.0:9090] [--domain ...] [--model ...] [--user <id>]  # 启动 orchestrator + 外部 /cron/fire 接收
```

机制：[scheduler-mechanism.md](scheduler-mechanism.md)（Scheduler）。

## Terminal（终端后端）

```bash
oneai terminal list                    # 列出可用后端（local / docker / modal / daytona）
oneai terminal exec --backend <name> "<command>" [--timeout 120] [--max-output 100000]  # 一次性执行命令
oneai terminal snapshot --backend <name>   # 快照会话状态（返回可恢复 id）
oneai terminal restore --backend <name> --id <id>  # 从快照恢复
oneai terminal cleanup --backend <name> [--hibernate]  # 拆除后端（--hibernate 停+保留可恢复；否则销毁）
```

机制：[CLAUDE.md — TerminalBackend](../CLAUDE.md)（ShellTool 的执行后端）。

## Embedding 服务

```bash
oneai embed generate "text" [--model ...] [--provider auto|openai|voyage|ollama|fastembed|openai-compat] [--api-key ...]
oneai embed batch "t1,t2" [同上选项]   # 批量生成
oneai embed list                       # 列出可用 provider + auto 探测链
oneai embed health [同上选项]          # 检查 embedding 服务健康
oneai embed dimension [同上选项]       # 查看模型向量维度
```

机制：[rag-mechanism.md](rag-mechanism.md)。

## WASM 沙箱

```bash
oneai wasm load <name> <file.wasm>     # 加载模块
oneai wasm run <name> [--input <json> | --input-file <path>]  # 执行模块
oneai wasm health [--name <name>]      # 模块健康检查
oneai wasm unload <name>              # 卸载模块
```

（另有 `oneai wasm stats` 资源监控统计。）机制：[wasm-mechanism.md](wasm-mechanism.md)（WASM）。

## MCP（客户端 + 服务端）

```bash
oneai mcp serve [--domain coding]       # 作为 MCP 服务器运行（兼容 Claude Code/Cursor）
oneai mcp list                          # 列出已配置 MCP 服务器
oneai mcp add <name> --transport stdio|sse|streamable_http [--command ...] [--url ...] [--args ...] [--enabled] [--lazy]
oneai mcp remove <name>                # 移除 MCP 服务器
oneai mcp connect <name>              # 测试连接并展示发现的工具
```

`--lazy`（Stage 5）让该 server 启动时不连、经 `tool_search` 触发 `McpLazyConnectTool` 按需连——连接后真工具浮给模型、trigger 自 vanishing。HTTP 传输的 server 走 OAuth 2.0 PKCE 全流程（`--manual` 改手动粘贴授权码，SSH/headless 友好；token 持久在 `~/.oneai/mcp_oauth/<server>.json`，401 自动刷新重试）。server 在 `tools/call` 中途反向问用户走 elicitation，经 `InteractionGate::McpElicitation` 点。工具以 `mcp__<server>__<tool>` namespaced 注册，每个 server 可带 `McpToolPermissions` 设 `PermissionLevel`/`ToolExposure`。机制：[mcp-mechanism.md](mcp-mechanism.md)。

## A2A（Agent-to-Agent 协议）

```bash
oneai a2a serve [--domain coding] [--port 8080]  # 启动 A2A 服务器，暴露 OneAI agent 能力
oneai a2a discover <url>               # 发现远程 A2A agent 能力
oneai a2a list                         # 列出已配置 A2A 端点
oneai a2a send <url> "<任务消息>"      # 向远程 A2A agent 发送任务
```

机制：[a2a-mechanism.md](a2a-mechanism.md)（A2A）。

## Gateway（消息网关）

```bash
oneai gateway serve [--bind 0.0.0.0:9090] [--domain ...] [--model ...] [--user <id>]  # 启动 webhook 服务端（飞书/企业微信/loopback）
oneai gateway channels                 # 列出已绑定通道（platform → session id）
oneai gateway autostart {install|uninstall|status}  # 管理 macOS LaunchAgent（登录即自启 supervisor+gateway）
```

机制：[gateway-mechanism.md](gateway-mechanism.md)（Gateway）。

## Supervisor（headless 监督 daemon）

```bash
oneai supervisor serve [--socket <path>] [--domain ...] [--model ...] [--user <id>] [--with-gateway] [--gateway-bind ...]  # 启动 daemon
oneai supervisor list [--socket <path>]  # 列出受管实例
oneai supervisor spawn <id> [--domain ...] [--model ...] [--user <id>] [--socket <path>]  # 拉起新实例
oneai supervisor stop <id> [--socket <path>]  # 停止实例
oneai supervisor status <id> [--socket <path>]  # 查询实例状态
oneai supervisor rpc <id> "<json>" [--socket <path>]   # 单次 RPC
oneai supervisor rpc-stream <id> [--socket <path>]     # 流式 RPC
```

机制：[supervisor-mechanism.md](supervisor-mechanism.md)（Supervisor）。

## Serve（引擎总线 sidecar）

```bash
oneai serve [--socket ~/.oneai/serve.sock] [--domain ...] [--model ...] [--user <id>]  # 启动引擎总线 sidecar
```

把一个 `AppSession` 经统一引擎总线暴露给**跨进程前端**（原生 App / IDE 插件）：socket 上写 `Directive` JSON 行、读 `EngineYield` JSON 行。UDS（Unix）/ named pipe（Windows）。区别于 `oneai supervisor serve`：supervisor 是实例注册 RPC（request/response `spawn/list/stop`），serve 是双向并发总线（任意时 directive ↔ 任意时 yield + 审批 `request_id` 关联），用分离 socket 故两者共存。机制：[bus-mechanism.md](bus-mechanism.md)（引擎总线）。

## Evolve（自演进）

```bash
oneai evolve run --seed <pack.yaml> --suite <name> [--max-generations 3] [--target 0.85] [--patience 2]   # 跑一代/多代闭环
oneai evolve report ~/.oneai/evolve/run-<ts>    # 离线检视产物
oneai evolve diff  ~/.oneai/evolve/run-<ts>    # seed vs frontier 配置 diff
oneai evolve lesson ~/.oneai/evolve/run-<ts>   # 跨代 lesson 日志
oneai evolve step  ~/.oneai/evolve/run-<ts> --suite <name>   # 续跑一代
```

GEPA 式外层演化循环——不动模型权重，只在 `DomainPackConfig`（7 层 pack）+ `AgentLoopConfig` 的文本/数值旋钮空间里变异，每代用真实 eval suite 打分、Pareto 多目标选前沿、lesson 合并携带进下一代。三道安全闸（`DomainPackValidator` + PermissionResolver 静态闸 + judge/candidate 分离）+ 两道回归闸（held-out 过拟合检测 + replay 确定性漂移）。机制：[self-evolution-mechanism.md](self-evolution-mechanism.md)（自演进）。

## Web UI

```bash
oneai studio [--port 3000] [--domain coding] [--model ...] [--user <id>]  # 启动 Studio Web UI（StateGraph 可视化 + Checkpoint 时间旅行）
```

机制：[studio-mechanism.md](studio-mechanism.md)（Studio）。

## 配置

```bash
oneai config show                      # 查看当前配置
oneai config init                      # 创建默认配置文件
```

## Reload（热重载）

```bash
oneai reload [--domain ...] [--model ...] [--user <id>]  # 不重启重读数据层（发现的技能、MCP 工具注册）
```

机制：[CLAUDE.md — DataLayerReloader](../CLAUDE.md)。
