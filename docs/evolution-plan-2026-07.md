# OneAI 架构演进计划（2026-07）

> 日期：2026-07-28
> 输入：`docs/gap-analysis-2026-07.md`（代码级差距，"文档说是、代码不是"）+ `docs/hermes-pi-inspiration.md`（Hermes-Agent / PI-Agent 深度研究启发）
> 方法：先核对两份文档声称的代码现状（grep + git log 取证），再据此把"修已宣称但没真生效的脊梁"与"借鉴两项目的先进设计"两条路线合并成一份按真实状态排序的演进计划。
> 结论先行：OneAI 的**结构宽度**已是护城河，**主执行路径的工程深度**经 2026-07-26 一轮修复已通电（gap P0 全清），下一步不是再加宽度，而是**先硬化+立发布纪律（Phase 1）**，再上**差异化与可达性（Phase 2-3）**，最后**精细化（Phase 4）**。

---

## 0. 现状核对（2026-07-28 实测，非读文档）

两份输入文档各带一份 P0-P3 路线图，但都基于较早快照。下表是本次对仓库实测后的真实状态：

| 轨 | 条目 | 状态 | 证据 |
|---|---|---|---|
| gap P0 #1 | 畸形 tool args 回喂自纠正 | ✅ 已修 | `74e375d`；`parse_tool_args` 单一 parse 站点 + 回喂 |
| gap P0 #2 | TokenBudget 真终止 | ✅ 已修 | `0416f19`；`AgentLoopConfig.token_budget` + 每轮 `record_usage` |
| gap P0 #3 | ContextManager 4 策略接主循环 | ✅ 已修 | `70530a0`；预推理块 `trim_for_model` |
| gap P0 #8 | 压缩抽取事实经 `archive_facts` | ✅ 已修 | `8bc0e6f`；`FactSink` trait |
| gap P0 #9 | 三层解析器 Layer2/Layer3 真接 | ✅ 已修 | `62b42a9`；`repair_tool_args` 默认 trait 方法 |
| gap P0 #4 | OTEL 真导出 OTLP + metrics 接 AgentLoop | ✅ 已修 | `83e7daf`；`HttpOtlpExporter` + `OtlMetricsProvider` 接线 |
| gap P0 #5/#6/#10 | RecoveryManager 真重试 / DegradationRule / Custom 边条件 | ✅ 已修 | `d35671e`/`22a4764`/`183d835` |
| **gap P1 — 退避 jitter** | RecoveryManager 退避加 jitter | ✅ 已修 | `error_recovery.rs:167-186` `compute_delay` 含 0..25% 抖动 |
| **gap P1 — ToolExecutor 输出尺寸上限** | 统一 tool-result 截断守卫 | ❌ 未做 | `executor.rs` 无 truncat/max_output，仍各工具 ad-hoc |
| **gap P1 — ShellTool 默认沙箱** | `ShellTool::new()` 默认接 sandbox backend | ❌ 未做 | `tool_interfaces.rs:112` `new()` 仍 regex-only |
| **gap P1 — ToolExecutor 接 PermissionProfile** | 统一权限路径（修 workflow 绕过 deny） | ❌ 未做 | `executor.rs:139-219` 仍只看 `risk_level()`，无 `PermissionProfile`/`deny_by_default` |
| **gap P1 — ThresholdGate 迁 PermissionLevel** |  | ❌ 未做 | `interaction_gate.rs:227,235,327` 仍 `RiskLevel` |
| **inspiration P0-1 — cache-stable prompt** | 范式后缀化，不重写 system prompt | ✅ 已修 | `apply_paradigm_switch` 标记范式尾部 + 仅 retain 被标记块，保留稳定前缀 |
| **inspiration P0-2 — check_fn Footprint gate** | 前置缺失则工具从 schema 零足迹排除 | ✅ 已修 | `Tool::service_available()` 默认方法 + `GatedTool`/`register_gated` + 三路径 `.filter` |
| **inspiration P0-3 — 供应链纪律** | 精确锁定 / cargo audit-deny / lockfile 闸 / OIDC / 隔离冒烟 | ✅ 已修 | `deny.toml`+`audit.yml`+lockfile-gate+`publish.yml`+`release-local.sh` |

**读取**：
- gap-analysis 的 P0（虚假安全感脊梁）**已全部通电**——这是 2026-07-26 的成果。
- gap-analysis 的 **P1 安全护栏**（1.4）与 inspiration 的 **P0（cache/check_fn/供应链）**（1.1/1.2/1.3）+ 1.5 衍生项**全部闭合**（2026-07-28）。Phase 1 完成。
- inspiration P1-P3（新能力）与 gap P2-P3（能力补齐/收尾）**基本正交**，可按"差异化→可达性→精细化"分阶段叠加。

---

## 1. 总体策略：两条路线合并为一个漏斗

两条输入路线图的性质不同：
- **gap-analysis** = "让已有的东西真正工作 + 安全"（向内、修信任）。
- **hermes-pi-inspiration** = "借鉴先进设计加新能力"（向外、加广度）。

合并原则：**先硬化再加宽**。新能力建在未硬化的基座上会放大风险（一个无输出上限的 ToolExecutor + 一个 serverless 终端 backend + 一个消息网关 = 攻击面爆增）。因此 Phase 1 把两条路线的"修硬伤"层合并做完，再叠加差异化与可达性。

漏斗：

```
Phase 1  稳定化 + 发布纪律   ← gap-P1 安全 + inspiration-P0 cache/check_fn/供应链   （低-中投入, 修信任+成本, 无新面）
Phase 2  差异化               ← inspiration-P1 闭环学习/supervisor/catalog            （中-高, OneAI 最大差异化）
Phase 3  可达性               ← inspiration-P2 网关/cron/serverless/自扩展/HF导出       （高, 新用户面）
Phase 4  精细化               ← inspiration-P3 行级diff/Gondolin/Api-Provider/hook分缝  （中-高, 综合+收尾）
                                                              + gap-P3 清死代码/SkillSelector
```

每条任务给 **What / Why / How / Effort / Fit**，并标注它**修复的是虚假安全感还是新增能力**。

---

## 2. Phase 1 — 稳定化 + 发布纪律（立即）

> 目标：把"已通电的脊梁"加固成"可信任的脊梁"，并立发布纪律。全部低-中投入、无新对外面、纯收益。必须先于 Phase 2-3 完成。

### 1.1 cache-stable system prompt + 范式后缀化（修 inspiration P0-1 + gap P1 #10 合并）✅
> **进度**：全完成（2026-07-28）。`apply_paradigm_switch`（`agent_loop.rs`）重写：范式尾部 system 消息以 `metadata["paradigm_tail"]` 标记，切换时 `retain` 仅移除被标记的尾部，**保留稳定前缀**（`build_system_prompt() + runtime_context_block()` = 身份/工具偏好规则/日期/web 指引）及其他合法 system 消息（反馈重试 prompt 等）。修两个 bug：(a) 范式切换让 Anthropic 前缀缓存每轮失效、成本翻倍；(b) `runtime_context_block`（日期+web 指引）连同被丢的正确性 bug。稳定前缀在会话期字节稳定（仅 session start 构建一次），缓存天然存活。1 新测（`e2e_scenario_4c`：断言切换后 `Current date and time`/`web_search`/身份 prompt 仍在 + 范式尾部并存）。取舍：未单独降时间戳日级——前缀仅 session start 构建一次已天然缓存稳定，非每轮重注。

### 1.2 `check_fn` Footprint gate on ToolRegistry + DomainPack（inspiration P0-2）✅
> **进度**：全完成（2026-07-28）。(a) `Tool` trait（`oneai-core/traits.rs`）加默认方法 `fn service_available(&self) -> bool { true }`——零足迹门，非 async（每轮热路径）；(b) `oneai-tool` 加 `GatedTool` 包装器（`ServiceCheck = Arc<dyn Fn()->bool + Send+Sync>`）+ `ToolRegistry::register_gated(tool, check_fn)`——注册级 check_fn，任意工具可条件隐藏无需自实现；(c) `build_tool_definitions_for_paradigm` / `build_tool_definitions` / `build_tool_definitions_for_state` 三路径均 `.filter(service_available)`，隐藏即从 schema 彻底排除 + warn log surface "已配置但前置缺失"；(d) Footprint Ladder（extend→skill→service-gated→plugin→MCP→core tool）写进 CLAUDE.md 作新能力决策准则。3 新测（2 oneai-tool 单测 + `e2e_scenario_4b` 端到端断言 gated-off 工具不进 schema、sibling 可用工具仍在）。取舍：未加 DomainPack `gated_tools` 配置层——trait 方法 + 注册级 check_fn 已覆盖机制，DomainPack 层留后续按需。

### 1.3 供应链纪律：精确锁定 + cargo audit/deny + lockfile 闸 + OIDC 发布 + 隔离冒烟（inspiration P0-3）✅
> **进度**：全完成（2026-07-28）。取舍：**不转 `=` 精确锁定**——`Cargo.lock` 已提交（workspace 发 binaries，reproducible builds 已保证），外部依赖全集中在根 `[workspace.dependencies]`；可复现性靠 Cargo.lock + cargo-deny，转 `=` 反成维护负担。落地：(a) 修内部 `oneai-*` path 依赖版本漂移 `1.0.0→1.1.0`（与 `[workspace.package]` lockstep，真发布侧 bug）；(b) `deny.toml` + `.github/workflows/audit.yml`（cron daily + PR，**单用 cargo-deny** 覆 advisories+licenses+bans+sources，不用 cargo-audit 避免双 ignore 列表漂移；首次跑修 3 个可 caret 升级的 advisory——crossbeam-deque 0.8.6→0.8.7、plist 1.9.0→1.10.0——其余 6 个带 rationale 忽略，均在 opt-in/未绑定路径）；(c) `ci.yml` lockfile-gate job（PR 改 Cargo.lock 未设 `ONEAI_ALLOW_LOCKFILE_CHANGE=1` 则拒）；(d) `.github/workflows/publish.yml`（tag `v*` 触发，复用 `scripts/publish_crates.sh`，`id-token:write` 留 Trusted Publishing，Path B token + Path A 文档化）；(e) `scripts/release-local.sh`（`cargo publish --dry-run` 每 crate = package+path-dep 重写+隔离构建+metadata 校验，冒烟后提示打 tag）。详见 CLAUDE.md「Supply-chain discipline」。

### 1.4 安全护栏补齐（gap P1 剩余）

> **进度**：1.4-a（ToolExecutor 输出尺寸上限，commit df96063）✅；1.4-b（ToolExecutor/workflow 接 PermissionProfile，关闭 workflow 绕过 deny_by_default 洞）✅；1.4-c/1.4-d 待做。

- **What**：
  1. ✅ **ToolExecutor 级输出尺寸上限**——`executor.rs` 加 `max_output_bytes`（默认 1MiB）+ `enforce_output_limit`，统一 tool-result 截断守卫。3 测。
  2. ✅ **统一权限路径**：`PermissionResolver` trait（`oneai-core`）+ `PermissionAction` 移至 core（domain re-export 保兼容）+ `PermissionProfile` impl trait；注入 `ToolExecutor`、`WorkflowExecutor`、`DirectProviderActionExecutor` 三条执行路径；`AppBuilder` 灌 merged DomainPack 的 `permission_profile`。修 workflow/StateGraph 工具步绕过 `deny_by_default` 的洞。6 测（3 ToolExecutor 单测 + 3 workflow e2e）。
  3. ✅ `ShellTool::new()` 默认沙箱 + 黑名单规范化命令解析 + 文件工具 `..` canonical-path：
     - 黑名单补 `rm -rf ~`/`$HOME`、`find ... -delete`/`-exec rm`、`curl|sh`/`| bash`（原仅挡 `rm -rf /`）。
     - 文件工具 7 处 `contains("..")` 改 `path_has_traversal`（`Path::components()` 真组件检测，修 `foo/..bar` 误杀）。
     - CodingPack 的 `ShellTool::new()` 改 `with_sandbox_backend(default_sandbox_backend(project_dir))`——生产默认真隔离（Seatbelt/Docker），测试用 `new()` 仍 regex-only 不受影响。
     - 4 新测（blocked hardened + 无误报 + traversal flags + 不误杀 `..bar`）。
  4. ✅ `ThresholdInteractionGate` 迁到 `PermissionLevel`：内部存 `Option<PermissionLevel>`，加 `new_with_permission_threshold` 构造器（RiskLevel `new` 保留向后兼容），`request()` 用 `permission_level` 取代 `risk_level` 决策。2 新测（permission 覆盖 risk / at-or-below auto-proceed）。

**1.4 全部完成**（4/4 子项）。gap-analysis P1 安全护栏层闭合。
- **Why**：现状 `executor.rs:139-219` 只看 `risk_level()` 不看 `PermissionProfile`，workflow 步骤绕过 domain 的 `deny_by_default`——权限两条路径分叉。无输出上限 → 一个无自截的 MCP/自定义工具长输出会在压缩触发前撑爆 context。ShellTool 默认无真隔离。
- **How**：`crates/oneai-tool` 的 `executor.rs`/`tool_interfaces.rs`/`interaction_gate.rs`。jitter 已在 `error_recovery.rs:167` 完成，本条只补其余。
- **Effort**：中。**Fit**：高，是 Phase 3 serverless backend/网关的前置（无它则攻击面爆增）。**类型**：修虚假安全感。

### 1.5 非中断式空响应重试保 `prompt_cache_policy`（gap P1 衍生）✅
> **进度**：全完成（2026-07-28）。空响应重试 `InferenceRequest`（`agent_loop.rs` ~5b empty-retry 块）由 `metadata: HashMap::new()` 改为带 `prompt_cache_policy` 的 map，重试仍命中前缀缓存。顺带修 StateGraph 节点推理路径（`AgentLoopGraphActionExecutor` 的 `InferenceRequest`）同病——也由空 map 改带 policy。1 新测（`e2e_scenario_4d`：断言主请求 + 重试请求 metadata 均带 `prompt_cache_policy`）。
- **What**：空响应重试时保留 `prompt_cache_policy` metadata（`agent_loop.rs:1618` 现丢失），让重试仍命中 prompt cache。
- **Why**：重试打不到缓存 = 重复成本，与 1.1 同源。
- **Effort**：低。**Fit**：高。**类型**：降成本。

---

## 3. Phase 2 — 差异化（近期）

> 目标：把 OneAI 从"带 skill 的代理"变成"随你成长的代理"（Hermes 最大差异化），并现代化 provider 抽象、解决原生 app 会话存活。这是 OneAI 最该做的差异化层，落在现有 SubAgent/DomainPack/FileWorkingStateStore 上，不新子系统。

### 2.1 闭环学习：cadence-fired reflection meta-tool + skill 生命周期 + curator（inspiration P1-1）

> **进度**：Stage A 完成（2026-07-28）。`SubAgentKind::Reflect` 变体 + Hermes 式 review prompt（frustration-as-signal / patch→umbrella→support-file→create / 反模式禁捕获环境失败）+ `AgentLoopConfig.reflection_cadence: Option<usize>`（默认 None=关，向后兼容）+ `AgentLoopObserver::on_reflection` hook。触发=迭代数跨 cadence 倍数（mid-run）AND `DirectAnswer` 交付后，且 `!interrupt_requested` + 无 pending interrupt。reflect 子代理经现有 `SubAgentFactory` spawn，继承父 provider（取暖）；其自身 `AgentLoop` 用 `SubAgentFactoryNone`（`delegate` meta-tool 自动从 schema 剔除=无递归 nudge，由 `available_kinds().is_empty()` 路径），`hard_max_iterations=16`，`context_manager=None`（不压缩），memory-only 工具白名单（`memory_search`/`core_memory_edit`/`archival_memory_insert`，strict 模式不 fallback 全集）。summary 经 `on_reflection` 上报 + trace log，**不**注入父会话（保父上下文干净，sub-agent 委派本意）。Footprint gate：memory 工具未注册则跳过 reflect。`AppBuilder::reflection_cadence(n)` 全链路（AppBuilder→App→AppSession→两条 AgentLoopConfig 构造路径）。6 新测（Reflect 单测 + cadence fire / DirectAnswer-only / interrupt 抑制 / 默认关 / summary 不污染父会话）。**取舍**：跨 session cadence 从 `FileWorkingStateStore` hydrate / `auxiliary.background_review` SmartRouter 路由便宜模型 = Stage C 范畴，本阶段不做（cadence 计数器在 LoopState 内存，每次 run 复位）。

> **进度**：**Stage B 完成（2026-07-28）**。skill 生命周期 + curator 全链路落地：
> - `oneai-skill/lifecycle.rs`：`SkillState`(Active/Stale/Archived, `#[non_exhaustive]`) + `SkillAuthor`(User/Agent/Bundled, derive Default) + `SkillMetadata`{state/use_count/last_activity_at/pinned/created_by/origin_note/created_at/archived_at} + `SkillMetadataStore`（`metadata.json` 持久 + `backups/<ts>.json.gz` 轮转快照 N=5，可 `rollback`）+ 纯 `apply_automatic_transitions`（30d→Stale/90d→Archived，pinned+Bundled+referenced 豁免，use==0 有 grace 窗口）+ `SkillLifecycleConfig`（原语，store 解耦于 domain）。
> - `oneai-skill/curator.rs`：`SkillCurator`{registry+store+referenced} — `run`（归档前先快照→可逆）+ `status`（registry×metadata join）+ `pin/unpin/archive/restore` + `backup/rollback`。归档引用中的技能只 warn 不阻断（破坏性 workflow 重写留后续 Stage）。
> - `oneai-domain/memory_profile.rs`：`SkillLifecyclePolicy`（stale_after/archive_after/auto_transitions/backup_count/storage_root/grace_unused）折进 `MemoryProfile` 作 layer-7 子配置（对标 `WorkingStatePolicy`），presets coding/assistant + merge（min 阈值/backup_count，OR auto，primary storage_root）。
> - `oneai-agent`：`skill_manage_tool`（archive/restore/pin/unpin/list）+ `SkillTool` 加 `with_metadata_store`（激活 `bump_use` + 拒绝 Archived）+ `SubAgentKind::Reflect` 白名单加 `skill_manage`（reviewer 可直接 patch 技能库，Hermes patch→umbrella→support-file→create 偏好序可执行）+ `maybe_run_reflection` footprint guard 放宽为（memory 三件套全注册）OR（skill_manage 注册）+ `AgentLoop` 持 `skill_metadata_store`，`build_skill_menu` 隐藏 Archived（退休下轮生效免重启）。
> - `oneai-app`：`AppBuilder.build()` 从 merged pack 的 `skill_lifecycle` policy（无 pack 时 fallback `coding()`，对标 working_state 的 200/50 fallback）构造 store+curator，root=HomeDir(`~/.oneai/curator/`) 因技能用量是用户习惯（不入 repo）；`seed_bundled` 仅首见钉住（用户 unpin 跨重建存活）；`App::register_skill_tools()` 统一注册 `SkillTool`(带 store) + `SkillManageTool`。`AppSession` 经 `.with_skill_metadata_store` 注入 loop。
> - CLI `examples/cli/cmd_curator.rs` + `Curator` 子命令（status/run/pin/unpin/archive/restore/backup/backups/rollback，对标 `pack`），provider-less App（curator 不需 LLM）。冒烟：status→backup→archive→run→restore→rollback 全绿。
> - 取舍：(1) `.json.gz` 快照而非 `.tar.gz`——保 crate 免引 `tar` 依赖（供应链纪律），能力（可回滚 N 份快照）等同；(2) 归档被 workflow/cron 引用的技能只 warn 不重写（破坏性重写留后续 Stage）；(3) `referenced` 集起手空（cron 是 Phase 3.2，未到；`set_referenced` 是刷新钩）；(4) 跨 session cadence hydrate = Stage C（见下）。共 1456 workspace tests 绿，fmt+clippy 清。

> **进度**：**Stage C 完成（2026-07-28）**——闭环学习收尾。两块：
> 1. **跨 session cadence hydrate**：新增 `TaskEventType::ReflectionFired`（`#[non_exhaustive]` 已就位）+ `TaskEventPayload::ReflectionFired { iteration: u64 }`（**累计**迭代数，使跨 session 边界可比较）+ `WorkingState.reflection_count`/`last_reflection_iter`（`#[serde(default)]`，向后兼容）。`working_state_store.rs` projector 折叠该事件（`Snapshot` arm 已 wholesale replace，计数随压缩存活）。`AgentLoop`：`LoopState.cadence_baseline`（new+from_conversation 双默认 0）+ `hydrate_working_state` 在 `state.working_state = Some(ws)` 后 hydrate `reflections_fired`/`cadence_baseline`（已是 run start/resume 单一 hydrate 点）+ cadence 检查改 **累计** `baseline+iterations` 且 `cum > baseline` guard（防 resume 时 baseline 自身是 cadence 倍数→误即发，即上一 session 已发的边界不重发）+ `maybe_run_reflection` 在 `reflections_fired += 1` 后 best-effort append `ReflectionFired{cum_iter}` 事件（store+task_id 绑定时；失败 swallowed 不破父 loop）。**取舍**：`iterations` 仍 per-run（不动预算/终止语义）；只 hydrate 累计计数 + baseline。
> 2. **LLM consolidation 默认关 opt-in**：`oneai-skill/curator.rs` 加 `consolidation_candidates()`（Active 非 pinned/Bundled/referenced）+ `apply_merge(umbrella, members, skills_dir)`（复用 `rollback` 的 SKILL.md 物化格式 frontmatter+body，注册 umbrella 作 `Agent` 作者，单份共享 backup 后 archive 各 member→整次 merge 可 `rollback <id>` 一步撤销；referenced member warn-skip）+ `MergeReport`/`MergeError`（manual Display+Error，对标 `RollbackError` 不引 thiserror）。**层序**：curator 在低 crate 不能 spawn provider，故 LLM 编排放 `oneai-agent/skill_consolidation.rs`——`run_consolidation(provider, curator, skills_dir, gen)` 构 Hermes 式归并 prompt（"几百个各捕获一次会话的窄 skill 是库的失败而非特性"，≥2 member，热门 skill 不动）→单次推理→`FuzzyJsonRepair::repair_and_parse` 解析 JSON proposals（复用 3 层防御 Layer 2，非手搓 JSON）→逐个 `apply_merge`，缺失 member 的 proposal 跳过。**绝不 cadence-fire**——`oneai curator consolidate` CLI 子命令是唯一 opt-in 入口（provider-backed，镜像 `cmd_run` 的 ProviderFactory wiring，写入 `~/.oneai/skills/` 供下次 discovery）。完整 `SubAgentKind::Consolidate` agent loop（max_iter 高）留后续。
> - 测试：oneai-persistence +1（projection）、oneai-skill +3（apply_merge 正/拒重写/skip referenced）、oneai-agent +1 e2e（`e2e_cadence_hydrates_across_run_resume`：run1 发 fire 落 `ReflectionFired` 事件→run2 hydrate 后 `last_reflection_iter` 必须越过 run1 末次 fire，断言 hydrate 破损会重发同边界导致不前进而失败）+ oneai-agent +3（consolidation 空/解析应用/缺 member skip）。共 **1464 workspace tests 绿**，fmt+clippy 清。

- **What**：
  1. `AgentLoop` 加 turn/iter cadence 计数器（`AgentLoopConfig`，默认 0=关，向后兼容；从 `FileWorkingStateStore` hydrate）。跨阈值、`DirectAnswer` 交付后、`not interrupted` 时，经现有 `delegate`/`SubAgent` spawn 一个 **reflect 子代理**，工具白名单只 `memory`+`skill_manage`，跑 Hermes 式 review prompt（frustration-as-signal、patch→umbrella→support-file→create 偏好序、反模式禁捕获环境失败）。复用 `InteractionGate::PostInfer` gate 触发。
  2. 给 OneAI skill 加 `SkillState`(Active/Stale/Archived)+`use_count`+`last_activity_at`+`pinned`+`created_by`(Agent/User/Bundled)。`SkillTool` 每次调 `bump_use`。纯函数 `apply_automatic_transitions`（30d/90d，pinned+cron-referenced 豁免，use==0 宽限）。永不删除只归档 + `curator_backup` tar.gz 快照留 N=5 + 可撤销 `rollback`。归并时重写 workflow/StateGraph 的 skill 引用。作 `MemoryProfile`（DomainPack layer 7）子配置（对标 `WorkingStatePolicy` 折进 layer 7 先例）。
  3. LLM consolidation **默认关 opt-in**。
- **Why**：OneAI 有 skill-creator 但全靠用户手动触发；无 cadence-fired 自主 review、无 skill 生命周期、无 curator、无 FTS5 跨会话检索原始 transcript。这是"随你成长"的最大缺口。
- **How**：reflect 子代理必须 (a) 继承父 provider 取暖、(b) `WriteOrigin::BackgroundReview` 标记、(c) **持久化隔离**——共享 session_id 取暖但硬关 `SqliteSessionStore` 写和压缩（对标 Hermes `_persist_disabled`/`compression_enabled=False`）、(d) 关递归 nudge、(e) `max_iterations=16`。`auxiliary.background_review.{provider,model}` 经 `SmartRouter` 路由便宜模型 + digest 重放。把学习策略 prompt 移植进 `skill-creator` skill body。CLI `oneai curator run/status/restore/pin` 对标 `pack validate`。
- **Effort**：中-高。**Fit**：高。**类型**：新增差异化能力。**前置**：1.1（缓存稳定，否则 reflect 子代理 fork 取暖失效）。

### 2.2 headless 监督进程 `oneai-supervisor`（inspiration P1-2）
- **What**：后台 daemon 监督长寿命 AgentLoop 进程，持久化 `~/.oneai/server/instances.json` 实例注册表，`recover_after_restart()` 重连。UDS（Unix）/ named pipe（Win）`~/.oneai/server.sock`，`spawn/list/stop/status/rpc/rpc_stream`。原生 app 作 IPC 客户端重连。
- **Why**：OneAI 原生 app（macOS/Win/iOS/Android/Harmony）后台/被杀就会话死。`FileWorkingStateStore` 持久化任务状态但不持久化"活会话重连句柄"。修"会话存活"问题。
- **How**：对标 `pi-server`。先做 in-proc supervised tokio task，进程隔离作 opt-in 后续。复用 `oneai-trace` OTEL（已通电）。可作 `oneai-studio` 的一个 mode。
- **Effort**：中。**Fit**：高，直击原生 app 痛点。**类型**：新增能力。

### 2.3 生成式 model catalog + 哈希校验 + 每模型 Compat 标志（inspiration P1-3）
- **What**：(a) build 时从 models.dev + provider `/models` 生成 `crates/oneai-provider/src/catalog/models.generated.rs`（`pub static MODELS: &[ModelEntry]`，内嵌 JSON）+ `.manifest.json`（schema 版本+结构哈希+每文件 SHA256）。`cargo xtask check-model-data` 重推导验哈希跑 CI。release 内嵌快照 `--offline`。(b) `ModelEntry` 扩展带 `reasoning/input_modalities/thinking_format/supports_strict/cache_retention`。(c) `compat.rs` 加 `OpenAICompat`/`AnthropicCompat`/`GeminiCompat` 标志集，从 `base_url` 探测，取代 `if provider=="ollama"` 分支。
- **Why**：OneAI 的 L3 `BUILTIN_MODEL_CONTEXT` 手维护且漂移；L2 probe 异步慢、依赖网络、首轮路径慢；能力硬编码 per-provider。OpenAI 兼容面在涨（Ollama/vLLM/LM Studio/Groq），各微妙破损。
- **How**：把生成式 catalog 作三层解析的 L3 权威替代，probe 降级可选覆盖（仅 catalog 缺失时）。`Api`/`Provider` 分缝是更大重构（留 Phase 4 P3-3），先上 Compat 标志 + catalog。
- **Effort**：中。**Fit**：高，现代化 provider 层、喂 SmartRouter 能力感知路由。**类型**：新增能力+修漂移。

### 2.4 记忆衰减 + 递归反思（gap P2 #16，喂 2.1）
- **What**：importance 阈值驱逐 archival（事实不再永久累积）；反思按相关性检索 prior 而非最近 N 条（Generative Agents 式"洞察引用洞察"反思树）。
- **Why**：闭环学习（2.1）依赖一个会衰减、会递归反思的记忆，否则 curator 归并无据、reflect 子代理只看最近 3 条。两者同源，应配套。
- **How**：`crates/oneai-memory` 的 `manager.rs`/`compression.rs`（`extract_and_archive` 已修通，可在此加衰减驱逐）+ `reflection_agent.rs` 改相关性检索。
- **Effort**：中。**Fit**：高。**类型**：补能力+喂 2.1。**前置**：gap P0 #8（已修）。

---

## 4. Phase 3 — 可达性（中期）

> 目标：让 OneAI 从"原生 app 单进程 UI 客户端"变成"随处可达的代理"（消息平台 + cron + serverless 终端），并打通自扩展环与训练数据飞轮。这是最大新用户面，建在 Phase 1 硬化基座上。

### 3.1 消息网关 `oneai-gateway` + ChannelDirectory + SessionSource + profile routing（inspiration P2-1）
- **What**：新 `oneai-gateway` crate（peer `oneai-a2a`）暴露 `MessagePlatform` trait（`connect/handle_message/send` + 能力标志作 default trait method）+ `PlatformRegistry`（延迟加载）+ 规范 `MessageEvent`+`SessionSource`。一个 `GatewayRunner` tokio 多路复用。`ChannelDirectory`（友好名→ID，定期重建）+ `tokio::task_local!` 带 SessionSource（防并发互踩）。`ProfileRoute` 表 (platform/guild/channel/thread)→DomainPack，特异性评分。
- **Why**：OneAI 原生 app 是单进程 UI 客户端；网关让 OneAI 变 Telegram bot/Discord bot/Slack app。最大新用户面。
- **How**：网关坐 `AppBuilder` 之上如 `oneai-a2a` server host。复用 `FilePersistence`/`SqliteSessionStore` 存跨 channel session origin。首 2-3 adapter（Telegram `teloxide`/Discord `serenity`/Slack）。profile routing 复用 `MergedDomainPack` + 把 `apply_paradigm_switch` 机制扩展到换整 pack。**能力标志作 default trait method**是关键可移植教训。
- **Effort**：高。**Fit**：高，落在现有 AppBuilder/A2A 范式上。**类型**：新增能力。**前置**：1.4（统一权限路径，否则多平台权限混乱）。

### 3.2 cron ABC + 外部 one-shot provider（inspiration P2-2）
- **What**：把 `InMemoryScheduler`（`crates/oneai-scheduler/src/scheduler.rs:27`，重启即死）提为一个 `CronScheduler` trait 的一个 provider（最小面 `name/start`，`fire_due/reconcile/on_jobs_changed` 安全默认）+ `FileJobStore`（JSONL，对标 `FileWorkingStateStore`）+ `parse_schedule("30m"/"every 2h"/cron/ISO)` + `deliver="origin"` + 外部 one-shot provider（`provision(job_id,fire_at,callback_url)` + axum `/cron/fire` JWT 验证）+ store 级 CAS at-most-once。
- **Why**：OneAI 调度器重启即死、无 NL 调度、无外部触发。cron 是触达真实世界的 agent 的顶级用户请求。依赖 3.1 网关做投递。
- **How**：`InMemoryScheduler` 留零配默认。`AppBuilder::cron_provider(...)` 加法。投递复用网关 `send()`。**ABC+orchestrator 模式**套到现有调度器。
- **Effort**：中。**Fit**：高。**类型**：新增能力。**前置**：3.1。

### 3.3 终端 backend ABC + Modal/Daytona serverless（inspiration P2-3）
- **What**：`oneai-tool` 抽 `TerminalBackend` trait（`execute/snapshot/restore/cleanup(hibernate:bool)`），`ShellTool` 持 `Arc<dyn TerminalBackend>` 而非直 `Command::new`。`LocalBackend`=现状零行为变，`DockerBackend`→Modal/Daytona。`cleanup(hibernate=true)` 是单 chokepoint（Modal snapshot+terminate、Daytona stop FS 保留）。`FileSyncManager` 同步 `~/.oneai`。
- **Why**：OneAI ShellTool 本地唯一。serverless 给 (a) 成本 (b) 隔离 (c) 跨平台一致 (d) 配网关 scale-to-zero。直接赋能移动端长跑 coding agent。
- **How**：spawn-per-call+session 快照模型直接移植。DomainPack `PermissionProfile` 可让 `Full` 风险命令自动路由 sandbox backend。先 Local=现状，加 Docker，再 Modal/Daytona。为 Phase 4 P3-2 Gondolin 预留 host。
- **Effort**：中-高。**Fit**：高，独立价值（隔离+成本+移动 coding）。**类型**：新增能力。**前置**：1.4（ShellTool 沙箱/权限）。

### 3.4 自扩展：`addedToolNames` + 数据层热重载（inspiration P2-4）
- **What**：(a) 工具结果加 `added_tool_names: Vec<String>`，`AgentLoop` 在工具批 finalize 后 diff `ToolRegistry` active 集，新注册工具盖到结果上，下轮前 `refresh_tool_registry()`，不换范式/不重启。`PermissionLevel::Standard` gate + DomainPack `ParadigmStrategies` 工具过滤刷新后重应用。(b) `/reload` 等价：会话中重读 DomainPack 数据层（skill markdown/MCP/MemoryProfile JSON/StateGraph），发 `reload` 事件，可由模型经控制工具触发。Rust DomainPack 仍编译期。
- **Why**：`addedToolNames` 是 PI 最紧自扩展环，OneAI 无。热重载是迭代 agent 行为的大 UX 胜场（macOS 笔记显示用户重启重派生状态很痛）。
- **How**：(a) `Tool` 结果类型加字段，turn 边界加 refresh。(b) `AppBuilder` 持 file-watched `ResourceLoader`，`reload_runtime` 控制工具排队 follow-up，快照 working-state→拆扩展 runtime→重读→`OnResume` 对账。
- **Effort**：中。**Fit**：中-高。**类型**：新增能力。

### 3.5 A2A server 接 axum + 真跑 AgentLoop（gap P2 #15，与可达性同层）
- **What**：`A2AServerHost` 真起 HTTP server（`host.run(port)` 实现），`handle_send_task` 真跑 AgentLoop 而非占位响应；补 streaming/push notification 真投递/auth 校验/`tasks/resubscribe`/TaskStore 持久化。
- **Why**：A2A server 是 OneAI "对外可达"的另一面（与网关对称：网关=inbound 消息，A2A=agent-to-agent RPC）。当前 card 声明 `streaming:true` 但 server 不能流（虚假广告）。属 gap P0 唯一未清项（明确留 P2）。
- **How**：`crates/oneai-a2a/src/server.rs:15` + `handler.rs:98` + axum。复用 3.1 的 axum/JWT 基建。
- **Effort**：中。**Fit**：高。**类型**：修虚假安全感（card 谎言）+ 新能力。**前置**：1.4（auth 校验）。

### 3.6 OSS 会话导出喂训练 `oneai session export-hf`（inspiration P2-5）
- **What**：CLI 子命令导 OneAI 会话（conversation + working-state 事件 + tool calls/results）为 HF-dataset 兼容 JSONL，正则脱敏（API key/token），`huggingface-cli upload`。配 `oneai-eval` 把导出会话当 eval case 重放（扩 `replay.rs`）。
- **Why**：OneAI 有原始材料（`FileWorkingStateStore` JSONL + conversations）但无真实会话捕获 pipeline。造训练数据飞轮 + 真实世界 eval 语料（超 SWE-bench curated 玩具任务）。配 Hermes trajectory 压缩（ShareGPT、protect head+tail、snap boundary 不落 tool turn）可作 SFT/RL 数据。
- **How**：`Conversation`/`Message` 已 serde-serializable。`secrecy` crate 脱敏。
- **Effort**：低-中。**Fit**：高，扩现有 eval/replay。**类型**：新增能力。

---

## 5. Phase 4 — 精细化 / 综合（远期）

> 目标：架构分缝与综合，独立于单功能，是 Phase 2-3 之后的结构性优化。Phase 4 同时收 gap P3 清死代码/未接项。

### 4.1 行级差分 TUI / 渲染合并定时器（inspiration P3-1）
- **What**：最小先上**合并定时器**（`render_requested`+`set_timeout` 去抖）；大胜场是 chat 面板绕 ratatui 全重绘、吐 ANSI diff 行包 `?2026` 同步输出（line 级而非 cell 级）。
- **Why**：OneAI TUI perf 笔记（`tui-render-perf-fix`/`stream-macOS-mainqueue-flooding`/`macos-streaming-freeze-lazyvstack`）反复重发现此理；macOS `StreamCoalescer 20fps` 是同思路再发明。PI 有成熟独立答案。
- **Effort**：低-中。**Fit**：中。**类型**：精细化。

### 4.2 Gondolin tool-override + Remote Operations 接口（inspiration P3-2）
- **What**：`oneai-tool` 定义 `BashOperations/ReadOperations/...` trait，`ShellTool`/`ReadTool` 持 `Box<dyn BashOperations>`；`ToolRegistry` 支持**按名覆盖**（当前只 add）；`ContainerizedCodingPack` DomainPack 提供 VM-backed 同名工具，`PermissionProfile` 保持 `Full`（VM 即边界）。
- **Why**：PI 极简 + OneAI 权限的诚实综合——auth 留宿主、工具副作用进 micro-VM、靠 tool-override。吸收 Gondolin，**不砍权限**（戒律）。
- **How**：`ToolRegistry` 加按名覆盖；3.3 的 `TerminalBackend` 已为它预留 host。
- **Effort**：中-高。**Fit**：中。**类型**：综合。**前置**：3.3。

### 4.3 `Api`/`Provider` 分缝（inspiration P3-3）
- **What**：把 OneAI `LlmProvider` trait 拆成线协议（`Api`）+ provider 身份，OpenAI 兼容 provider 作配置行对一个 `openai-responses` Api impl。
- **Why**：2.3 的 Compat 标志是权宜，这是根治——加 Ollama/vLLM/LM Studio/Groq 不再要新 trait impl。
- **Effort**：高。**Fit**：中。**类型**：架构重构。**后续**：2.3。

### 4.4 `observe`/`on` hook 分缝 + per-event-type 突变语义（inspiration P3-4）
- **What**：`InteractionGate` 拆 observe（只读）/on（参与语义）两通道，每事件类型自带 result 型 + merge 策略（phantom-type），新增 `before_provider_request`/`before_compact`。`InteractionGate` 降为默认 impl 注册 `on(...)` handler，向后兼容。
- **Why**：现有 5 决策点已通电但语义混在 match 臂；hook 分缝让扩展点不 speculative（戒律：无消费者不加 hook）。
- **Effort**：中-高。**Fit**：中。**类型**：架构分缝。

### 4.5 纯循环 / 有状态外壳分缝（inspiration P3-5）
- **What**：把 `AgentLoop` 的纯循环抽成对快照的 generator，状态移到有状态外壳，`continue`/`shouldStopAfterTurn`/`prepareNextTurn` 变回调。
- **Why**：PI 的分缝让 continue/retry/轮间突变成一行回调；OneAI 把范式/delegate/StateGraph 塞进 match 臂。架构迁移，独立功能。
- **Effort**：高。**Fit**：中。**类型**：架构迁移。

### 4.6 收尾清死代码 + SkillSelector 真工作（gap P3）
- **What**：(a) 删 `mcp_tools.rs` 死桩副本、遗留 `ltm_entries`/`ShortTermMemory`/`LongTermMemory`（或 P3-1 稳定性承诺下显式 deprecate）。(b) `SkillSelector` embedding 真工作 + skill 版本/依赖/trust 边界。(c) StateGraph 并行分支（frontier）；WorkflowDag 改 BTreeMap 恢复确定性。
- **Why**：gap P3 收尾，降维护陷阱。Team/Swarm/Handoff 已在 `c8bedbe` 整层移除（无需再做）。
- **Effort**：中。**Fit**：中。**类型**：清死代码。

---

## 6. 戒律（抄 PI/Hermes 时守的边界）

承袭 inspiration §5，多数与 OneAI 已有纪律对齐：

1. **不抄 PI 的"无权限系统"**。OneAI 三层权限 + InteractionGate 是优势。吸收 Gondolin 综合（4.2），不砍权限。
2. **不把第三方产品塞进 core 树**。observability 后端/vendor SaaS/analytics dashboard 作独立 plugin 仓装进 `~/.oneai/plugins/`，不在 `crates/`。
3. **不写 speculative 基础设施**。无具体消费者的 hook/callback/扩展点不加——加 hook 容易，插件依赖后移除难（消费者即使分开发布也算非 speculative）。4.4 在此约束下做。
4. **不留 lazy-reading 逃生口**。对 agent 必须全文读的工具（skill/prompt/playbook）不加 `offset/limit` 分页。
5. **不用 env 变量做非密配置**。`.env` 只放凭证；超时/阈值/feature flag/显示偏好走 `config.yaml`/DomainPack。
6. **不写 change-detector 测试**。测断言不变量（"两数据必须如何关联"），非冻结当前值（模型列表/枚举数/版本字面量）。2.3 的 catalog 哈希校验是结构哈希非值冻结。
7. **不让"修复"毁掉它保护的功能**。读原 commit 意图（`git log -p -S`）再限制行为。
8. **不在 mid-conversation 破缓存**（与 1.1 一致）：mutate 过去上下文 / 换工具集 / 重建 system prompt 是禁忌，唯一例外是压缩。
9. **不并行多个 memory provider**（OneAI 已守：至多一外部 provider），防 schema 膨胀 + 后端冲突。
10. **不新加宽度先于硬化**（本计划核心策略）：Phase 2-3 的新能力必须待 Phase 1 完成。

---

## 7. 需要决策的架构张力点

承袭 inspiration §6，结合现状核对后仍需用户拍板：

1. **paradigm-switch vs cache**（1.1）：范式指令走 user 消息 vs 可丢弃尾部 system vs 只换工具不换 prompt？三者对缓存友好度递增、对范式表达力递减。**推荐**：稳定前缀 + 尾部追加 + 工具过滤，范式 prompt 作尾部 user 注入。
2. **DomainPack vs Footprint Ladder**（1.2）：`check_fn` 作 DomainPack Tool 层字段还是作 `ToolRegistry` 全局机制？**推荐**：两层都有——Tool 层声明 gated_tools，ToolRegistry 统一执行过滤。
3. **curator 是 DomainPack 层还是独立 crate**（2.1）？**推荐**：作 `MemoryProfile`（layer 7）子配置 + `oneai-skill` 内实现，对标 `WorkingStatePolicy` 折进 layer 7 先例。
4. **网关是 `oneai-app` 之上 adapter 还是独立 crate**（3.1）？**推荐**：独立 `oneai-gateway` crate（peer `oneai-a2a`），坐 AppBuilder 之上，与 A2A server host 对称。
5. **生成式 catalog 取代 L3 还是叠加**（2.3）？**推荐**：取代 L3 为权威，L1 用户 config 仍可覆盖，L2 probe 降级为可选（仅 catalog 缺失时）。
6. **Phase 1 内部排序**：1.1（cache）与 1.4（安全护栏）哪个先？**推荐**：1.4 先——它是 Phase 3 攻击面前置；1.1 与 2.1（reflect 子代理 fork 取暖）强耦合，可与 2.1 配套。1.2/1.3/1.5 任意时序。

---

## 8. 不做什么（本计划范围外）

- **不重写 AgentLoop 为纯循环外壳分缝**于 Phase 2-3 期间（4.5 是 Phase 4 架构迁移，提前做会动摇已通电的范式/delegate/StateGraph 路径）。
- **不在这版计划里加 computer-use/Playwright/git 工具**——这些是工具层扩展，应作 DomainPack/skill/plugin 走 Footprint Ladder（1.2 落地后），不进 core。
- **不在这版计划里做分布式 trace 传播**以外的可观测性后端——OTEL 已通电（gap P0 #4），新后端作独立 plugin 仓（戒律 2）。

---

## 附：与两份输入文档的映射

| 本计划 | gap-analysis 条目 | inspiration 条目 |
|---|---|---|
| 1.1 | P1 #10（apply_paradigm_switch 保 runtime_context） | P0-1 |
| 1.2 | — | P0-2 |
| 1.3 | — | P0-3 |
| 1.4 | P1 #6/#7/#8/#9（jitter 已修，余未做） | — |
| 1.5 | P1 衍生（空响应重试丢 cache policy） | — |
| 2.1 | — | P1-1 |
| 2.2 | — | P1-2 |
| 2.3 | — | P1-3 |
| 2.4 | P2 #16 | — |
| 3.1 | — | P2-1 |
| 3.2 | — | P2-2 |
| 3.3 | — | P2-3 |
| 3.4 | — | P2-4 |
| 3.5 | P2 #15（A2A HTTP server，gap P0 唯一未清） | — |
| 3.6 | — | P2-5 |
| 4.1 | — | P3-1 |
| 4.2 | — | P3-2 |
| 4.3 | — | P3-3 |
| 4.4 | — | P3-4 |
| 4.5 | — | P3-5 |
| 4.6 | P3 #18/#19/#20（Team/Swarm 已移除） | — |

gap-analysis P0 #1-#10 已在 2026-07-26 全部清零（见 memory `gap-p0-fix-progress`），本计划不再列入。
