# OneAI 自演进系统机制白皮书

> 五段闭环的外层驱动器：轨迹采集 → EDD 评分 → Minimal Subgraph 诊断 → GEPA Pareto 变异合并 → 重跑，外加 E5 安全护栏（DomainPackValidator + PermissionResolver 静态闸 + held-out 全 suite 回归闸 + replay 确定性回归闸）+ 跨代 lesson 记忆 + CLI 全套（run/step/report/diff/lesson）。

> 版本：对应代码库 1.1.0 线。本文基于对 `crates/oneai-evolve`、`oneai-domain`、`oneai-eval`、`oneai-app` 源码的逐文件审阅撰写，所有机制均标注 `file:line` 以便核对。设计依据见同目录 `docs/self-evolution-system-2026-08.md`。

---

## 0. 一句话概括

OneAI 的自演进是一个 **「黑盒 API 上的 GEPA 式外层演化循环」**：不动模型权重，只在 `DomainPackConfig`（7 层声明式 pack）+ `AgentLoopConfig` 的文本/数值旋钮空间里变异，每代用真实 eval suite 打分、Pareto 多目标选前沿、lesson 合并携带前沿进下一代，跑到收敛/预算/停滞。E5 在变异产物落地前叠了三道安全闸 + 两道回归闸，并把闭环做成可离线检视的 CLI 工具集。

---

## 1. 架构总览：五段闭环 ↔ 模块映射

```
   ┌────────────────────────────────────────────────────────────────┐
   │  EvolutionLoop (oneai-evolve/src/loop_runner.rs)              │
   │  外层驱动：for gen in 0..max_generations { … } 跑到收敛        │
   └───────┬───────────────────────────────────────────┬──────────┘
           │ ① 轨迹采集 (live, full-suite)               │ ④ GEPA 变异+Pareto
           ▼                                              ▼
   ┌──────────────────────┐   ┌────────────────────┐   ┌──────────────────────┐
   │ TrajectoryCollector   │   │ EDD 评分          │   │ GepaOptimizer         │
   │ (trajectory_collector │   │ EvalResult 三轴   │   │ vary→score→select→   │
   │  .rs) 每case新App+    │   │ pass/tokens/lat   │   │ merge（gepa.rs）      │
   │  RecordingProvider   │   │ (eval_runner.rs)  │   │                       │
   └──────────┬───────────┘   └─────────┬─────────┘   └───────────┬───────────┘
              │ ② (Trajectory,TraceTree) │ ③ FailedCase            │
              └──────────────┬──────────┘                          │
                             ▼                                     │
                    ┌────────────────────┐                         │
                    │ ② Minimal Subgraph  │ ⑤ merge → next_gen base │
                    │  诊断 (subgraph.rs) │   + LessonsLog 跨代记忆 │
                    │  反向BFS suspect    │   (lessons.rs)          │
                    │  ParamRef + critique│                         │
                    └────────────────────┘                         │
                                                                   │
   E5 安全/回归闸（最终代）:                                        │
   ┌──────────────────────────────────────────────────────────────┐
   │ DomainPackValidator (validate_candidate) ─ 结构+语义校验      │
   │ permission_safety_check ─ 不许把 require_confirmation/       │
   │   deny_by_default 的工具降级到 auto_approve                   │
   │ held-out 全 suite 回归闸 ─ frontier 在全 suite 重跑，         │
   │   held_out < train subset pass = 过拟合信号                    │
   │ replay 确定性回归闸 ─ 数值变异的轨迹 frozen-replay，          │
   │   tool_calls_match 断言行为未漂移（语义变异跳过）             │
   └──────────────────────────────────────────────────────────────┘
```

数据流：每代 `run_single_generation`（`loop_runner.rs`）采集 base 的 full-suite 轨迹 → EDD 评分 → 抽失败 case → 诊断归因到 suspect `ParamRef` → 在 case 子集上 vary K 候选 + Pareto 选前沿 →（最终代）held-out + replay 闸 → lesson 合并携带前沿进下一代。落盘：`<root>/evolve/run-<ts>/` 下 `seed.json` + `report.json` + `lessons.jsonl` + `frontier-gen<N>.json` + `case-<id>.jsonl` + `diagnosis-<id>.json`。

---

## 2. 变异基质全图（CandidateConfig 可寻址字段）

变异**只动 spec 空间**（设计稿 §0.1：黑盒 API，权重优化不可用）。`CandidateConfig`（`candidate.rs`）= `DomainPackConfig`（7 层，全 serde-able）+ `AgentLoopOverlay`（generation 旋钮）+ `skill_overrides`。

| 轴 | `ParamRef` 寻址 | 类型 | hack 面 | 落地 |
|---|---|---|---|---|
| `system_prompt` | `PackSystemPrompt` | 自由文本 | 高 | E3 |
| `tool_decorators[name]` | `PackToolDecorator` | 自由文本 | 高（verb 矛盾守卫） | E3 |
| `tools[name]` add/remove | `PackTool` | 离散集合 | 中 | E3 |
| `memory.recall.top_k` | `PackRecallTopK` | 数值 | 低 | E4 |
| `memory.extraction_schema` add/remove | `PackExtractionSchema` | 列表 | 低 | E4 |
| `compression / context / paradigm` | `PackCompressionField` 等 | 标签 | 中 | E3 后期（次轴） |
| `loop_overlay.thinking_budget / hard_max_iterations / token_budget` | `Loop*` | 数值 | 低 | E3+ |
| `permission_profile.*` | `PackPermission` | 离散 | 高（headless 无信号） | E5 慎/低优先 |

`ParamRef::from_path`（`subgraph.rs:152`）只解析 E3/E4 已落地轴；未知 path → drop + warn（不 panic）。`apply_patch`（`gepa.rs`）按 op 应用 + bound 检查（如 `recall.top_k ∈ [1, 128]`）+ reward-hacking 守卫（`semantic_guard_decoration`：装饰不得与工具 verb 矛盾）。

---

## 3. 五段闭环逐段机制

### ① 轨迹采集 — `TrajectoryCollector`（`trajectory_collector.rs`）
每 candidate 起全新 `App`（`CandidateConfig::build_app` 走 `DomainPackSpecFile::validate_and_build` → `AppBuilder::domain_pack`），provider 包 `RecordingProvider`（`oneai-eval`）以捕获 `(Trajectory, TraceTree)` + `EvalResult`，逐 case 跑 suite。**Live，非 replay**——语义变异候选不可 replay（设计稿 §3.3 难点 C）。

### ② EDD 评分 + Minimal Subgraph 诊断 — `subgraph.rs`
`FailureExtractor` 选低分 case → `SubgraphDiagnostician`（默认 `HeuristicDiagnostician`，确定性、无需 LLM；opt-in `LlmDiagnostician` 复用启发式核心的 `suspect_params`+`subtrace`，仅用 judge 重写 `critique`）。诊断走 span 树反向 BFS 找影响失败路径的最小 `ParamRef` 子集，尾 N 轮兜底。诊断结果持久化 `diagnosis-<id>.json` + 内联进 `report.json`。

### ③ GEPA 变异 + Pareto 选择 — `gepa.rs`
`LlmVariationOperator` 用**独立** variation provider（judge/candidate 分离，设计稿 §6.3）emit JSON patch-list → `apply_patches` → `validate_candidate`（`DomainPackValidator`）→ `permission_safety_check`（E5，下节）。失败候选 drop + warn，不 panic。存活候选在 case 子集上评分（`select_case_subset`：失败 case + 首尾 + 原序填充），三轴 Pareto（`pass_rate↑ / tokens↓ / latency↓`）选非支配前沿。

### ④ lesson 合并 + 跨代记忆 — `lessons.rs`
`LessonMerger`（默认 `BestFrontierMerger`，取前沿最优）把前沿合并成下一代 base，`LessonsLog` 跨代 append `lessons.jsonl`（每代一行：gen + base/frontier pass + axes + lessons_text）。收敛/停止四条件（设计稿 §4 E4）：frontier pass ≥ target / 累计 token ≥ cap / 停滞 ≥ patience / 代数 cap。

### ⑤ 重跑 — 每代 base 都 live 重跑 full suite
前沿合并后成为下一代 base，循环回 ①。中间代只落 lesson 行（省盘 + 诊断在内存喂下一代变异）；最终代才持久化轨迹 + 诊断 + 跑 E5 闸。

---

## 4. E5 安全护栏 + 回归闸

### 4.1 `DomainPackValidator` 静态闸（`gepa.rs::validate_candidate`）
变异产物落地前必过 `DomainPackSpecFile::validate`（结构 + 语义：未知工具名 / decay 越界 / 权限层空规则 / tool_decorators 引用未声明工具等）。校验不过的候选直接丢弃 + log，不 panic。

### 4.2 `permission_safety_check` — PermissionResolver 三路径静态闸（`gepa.rs`）
PermissionResolver 运行时三路径 = `deny_by_default → permission_overrides → auto_approve`（`oneai-tool` 解析顺序）。E5 把它做成 pack 级静态比对：candidate 的 `auto_approve` 集合**不得**包含 seed 在 `require_confirmation` ∪ `deny_by_default` 中的工具——即不允许"安全回退"（变异把一个危险工具从需确认降到自动批准）。收紧允许，只拦放宽。当前无 patch op 直接产 permission 变异（`PackPermission` 是 E5 慎/低优先，`from_path` 不解析），故此闸是 forward-looking 守卫——当 permission 轴变异落地时它已就位。

### 4.3 held-out 全 suite 回归闸（`loop_runner.rs::run_single_generation`，最终代）
变异评估用 case 子集（省钱、快收敛），但 frontier 可能过拟合子集。E5 闸：最终 frontier 选定后，在**全 suite**（非子集）重跑一次 `collect_runs(frontier, full_suite)`，pass_rate 记入 `GenerationSummary.held_out_pass_rate`。`held_out < frontier subset pass_rate` 即过拟合信号——`report.to_summary` 高亮 `⚠ overfit`。这是设计稿"训练/留出 case 分离"的具体化：不是少采样，而是分级采样。

### 4.4 replay 确定性回归闸（`loop_runner.rs`，最终代）
数值类变异（`recall.top_k` / `thinking_budget` / `hard_max_iterations` / `token_budget`，即 `is_replay_eligible` 判定：`system_prompt` + `tool_decorators` 未变）落地后，取 frontier 的 held-out 轨迹做 `replay_trajectory_with`（`oneai-eval::replay`）——用 frozen 响应重跑，断言 `tool_calls_match()`（tool-call 序列一致 + 不超 recorded 迭代数）。`FrontierRecord.replay_deterministic = Some(true/false)`。语义变异（system_prompt/decorators 变）跳过 + log（设计稿 §6.4：replay 只对非语义变异有效）。

> **replay 的 infra 边界**：现有 `replay_trajectory_with` 建的是 no-pack App，故只复现 direct-answer 轨迹（无 tool call）；tool-call 轨迹会因 no-pack 产生假阳性 mismatch，故 tool-call 轨迹跳过（`None`）+ log。这是 honest 的范围声明——pack-aware replay 留后续。

### 4.5 reward-hacking 分层防护（设计稿 §6.6）
1. **训练/留出 case 分离**（4.3 held-out 闸）——变异评估用训练子集，收敛判定用全 suite。
2. **tool_decorators 语义守卫**（`semantic_guard_decoration`）——装饰不得与工具 verb 矛盾。
3. **双 LLM 非对称**（judge 与 candidate 不同 provider，CLI `--judge-model`）。
4. 约束枚举/数值轴（MemoryProfile 的 recall/decay/core_budget）天然 hack 面小。

---

## 5. CLI 全套（`oneai evolve`，E5）

| 命令 | 作用 | 只读? |
|---|---|---|
| `run` | 跑一代/多代闭环，落盘 report + lessons + 轨迹 | 写 |
| `step <run-dir>` | 续跑一代（`run_one_more`）：读 report.json 定位 gen + no_optimize，读最新 frontier-gen{N}.json 或 seed.json 作新 base，重建 LessonsLog 跨边界，同 run_dir 追加 | 写 |
| `report <run-dir>` | 打印 report.json（`EvolutionReport::to_summary` / json） | 只读 |
| `diff <run-dir> [--gen N] [--seed file]` | `config_diff`：seed vs frontier 的结构化变更（按层列变更字段，`numeric_only` 标 replay 资格） | 只读 |
| `lesson <run-dir>` | 打印 lessons.jsonl 每代 frontier pass + 停滞计数 | 只读 |

`run`/`step` 的 `--judge-model` 接独立 variation provider（设计稿 §6.3）；缺省时 candidate provider 兼任（smoke harness，生产用库 API `EvolveRunArgs::with_variation_provider` 分离）。

---

## 6. 关键设计决策与权衡

1. **GEPA 式 reflect 而非 TextGrad 式 backward**：OneAI trace 是 span 树不是计算图，强行套 backward 要造全套图 IR；GEPA 的"采样轨迹→自然语言 reflect→改 prompt→Pareto 合并"与 span 树诊断天然对齐，多目标直接复用 SWE-bench 三轴。judge 用更强模型借 TextGrad 的双 LLM 非对称。
2. **case 子集 vs 全 suite**：变异评估用子集（省钱、快收敛），收敛判定 + held-out 闸用全 suite（防子集过拟合）——GEPA "fewer rollouts" 的分级采样具体化。
3. **replay 只对非语义变异有效**：明确写进文档——改 prompt/compression 的候选必须 live，replay 只服务数值旋钮的快速行为漂移检测。混用得假阳性。
4. **不收敛的退路**：连续 N 代无提升 → 输出"当前 frontier + 剩余 suspect_params 清单 + 人工建议"后停，不烧 token。
5. **temperature/top_p 在 eval 循环冻结为 0**：eval 优化确定性策略；调 temp 须多采样平均才公允（贵），首版不做。故可变异旋钮 = thinking_budget / hard_max_iterations / token_budget / recall.top_k；temp/top_p 固定。
6. **extraction_schema 是"压缩信息丢失"的另一半旋钮**：FactExtractor 的 LLM prompt 由 `MemoryProfile.extraction_schema` 派生，改 schema 直接改压缩时抽什么事实进 LTM——与 `compression_template.preserve_fields` 是同一故障模式的两个抓手。这是 MemoryProfile 必须纳入基质的技术根据。

---

## 7. 供应链与边界

- **零新外部依赖**：全 `oneai-*` workspace crates（`oneai-evolve` 依赖 core/domain/eval/agent/app）。
- **不改 Rust 代码**：变异只动 spec 空间；诊断指向"需改代码/加工具"→ 输出建议工单到 report，不自动执行。
- **`#[non_exhaustive]`**：所有对外 enum（`PatchOp` / `DiffEntry` / `EvolutionConfig` / `FrontierRecord` 等）按 v0.2.0 稳定性承诺加。
- **三件套 + deny**：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo deny check` 全绿（供应链戒律）。

---

## 8. 验收（E5 全部完成）

- ✅ CLI 全套 `run/step/report/diff/lesson`。
- ✅ 安全护栏：`validate_candidate`（DomainPackValidator）+ `permission_safety_check`（PermissionResolver 三路径静态闸）；变异不自动改 Rust 代码；judge/candidate 模型分离（`--judge-model`）。
- ✅ 回归闸：held-out 全 suite（`held_out_pass_rate`）+ replay 确定性（`replay_deterministic`，数值变异）。
- ✅ 文档：本文 + README 自演进章节（EN 镜像）+ memory 条目。
- ✅ 测试：`e2e_e5`（负测：非法 patch 丢弃；过拟合回归：held-out < train；replay 闸：数值→Some(true)/语义→None；step 续跑 gen 递增）+ `diff.rs` 单测。
