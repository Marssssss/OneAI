# OneAI 自我演进系统实施计划

> 日期：2026-08-06
> 状态：E0–E5 全部落地（E5 = CLI 全套 + 安全护栏 + held-out/replay 回归闸 + 文档）；设计稿与实现同步
> 范围：在 OneAI 中实现"轨迹采集 → EDD 评分 → Minimal Subgraph 诊断 → GEPA Pareto 合并优化 → 重新跑轨迹"的闭环自演进系统
> 前置调研：见上一轮 deep-research 结论（DSPy/GEPA/TextGrad/Trace/ADAS/Voyager/Reflexion 六家先验）。本计划只落地与 OneAI 现有架构对齐的子集。

## 0. 设计原则与硬约束

1. **黑盒 API → 只在 prompt/spec 空间变异**。OneAI 接 OpenAI/Anthropic/Ollama 黑盒 API，权重优化（DSPy BootstrapFinetune/GRPO、ADAS-in-principle 的权重子集）**不可用**。变异对象限定为 `DomainPackConfig`（string/Vec<String>/HashMap）+ `AgentLoopConfig` 的文本/数值旋钮 + skill 文本。这正好对齐 TextGrad 的 `Variable` 与 GEPA 的 prompt-update。
2. **必须有 ground-truth metric 兜底**。"LLMs Cannot Self-Correct Reasoning Yet" 一族批判证明：纯 LLM-as-judge、无外部信号时会震荡或净增益为零。所以 EDD 评分的"低分筛选"必须以客观 metric（ExactMatch/Trajectory/SWE-bench resolve）为主，LLM-judge 只做"诊断归因"，不做"是否改进"的判定。
3. **不自动改 Rust 代码**（不走 ADAS 路线）。reward hacking + token 成本 + 执行不可信代码风险太高。代码层演进由人介入，系统只输出"建议改 X"的工单。
4. **复用优先**：EvalRunner 已是评分器、Trace 已是轨迹源、DomainPackSpec 已是变异基质、Trajectory replay 已是回归闸、SWE-bench 三轴已是多目标基底——能复用的不重造。
5. **供应链戒律不变**：零新依赖（用现有 oneai-trace/eval/domain/skill/app），`#[non_exhaustive]` 加到新公开枚举，fmt+clippy(-D)+test+deny 四件套。

## 1. 架构总览：五段闭环 ↔ OneAI 模块映射

```
        ┌─────────────────────────────────────────────────────────────┐
        │  EvolutionLoop (新, oneai-evolve crate, 坐 app 下 同 studio)   │
        │                                                              │
        │  generation N 的 CandidateConfig                             │
        │      │                                                       │
        │      ▼  ① 轨迹采集                                            │
        │  TrajectoryCollector                                         │
        │   = RecordingProvider + InMemoryCollector                    │
        │   产物: Trajectory{input,responses,tool_calls,iter}           │
        │         + TraceTree (层级 span 树)                            │
        │      │                                                       │
        │      ▼  ② EDD 评分                                           │
        │  EvalRunner.run(suite)  ← 复用, 不改                          │
        │   产物: EvalReport → 每 case EvalResult{                      │
        │          scores, trace_metrics, EfficiencyProfile,           │
        │          usage, actual_output }                              │
        │      │                                                       │
        │      ▼  FailureExtractor (新, 极薄)                           │
        │   按 metric 阈值筛低分 case → Vec<FailedCase{                 │
        │     case, result, trace_tree, trajectory }>                  │
        │      │                                                       │
        │      ▼  ③ Minimal Subgraph 诊断 (新, 核心)                    │
        │  SubgraphDiagnostician                                       │
        │   输入: FailedCase + CandidateConfig 的"参数清单"             │
        │   做法: 在 TraceTree 上提取"变异参数 → 失败输出"的           │
        │         最小因果子树 (Trace MSP 风格, 非梯度)                  │
        │   产物: Diagnosis{                                            │
        │     suspect_params: Vec<ParamRef>,  // 指向 DomainPackConfig  │
        │                                     // / AgentLoopConfig 字段 │
        │     subtrace: TraceSlice,         // 裁剪后的局部轨迹         │
        │     critique: String }           // LLM-judge 自然语言诊断   │
        │      │                                                       │
        │      ▼  ④ GEPA Pareto 合并优化 (新, 核心)                      │
        │  GepaOptimizer                                               │
        │   a. 变异: 按 suspect_params 让 LLM 生成 K 个候选 patch        │
        │           (改 system_prompt / compression preserve_fields /  │
        │            tool_decorators / paradigm trigger / temperature) │
        │   b. 校验: DomainPackSpecFile::validate_and_build (现成)       │
        │   c. 评分: 对 K 个候选重跑 ①②(子集 case, 省钱)                │
        │   d. Pareto: 多目标=(pass_rate, tokens, latency) 非支配排序   │
        │   e. lesson 合并: 取 Pareto 前沿互补 lesson 拼下一代          │
        │   产物: generation N+1 的 CandidateConfig + LessonsLog       │
        │      │                                                       │
        │      ▼  ⑤ 重新跑轨迹 (回 ①, 喂 N+1 config)                     │
        │   收敛条件: pass_rate 达目标 OR max_generations OR            │
        │            budget.remaining() < 阈值                          │
        └─────────────────────────────────────────────────────────────┘
```

**crate 选址**：新建 `oneai-evolve`，依赖关系 `oneai-app` + `oneai-eval` + `oneai-domain` + `oneai-trace` + `oneai-skill`。位置与 `oneai-studio`/`oneai-supervisor` 同层（坐 app 下，**不加 AppBuilder 方法**，由 CLI 驱动 + trait seam，对齐 studio/supervisor 的戒律）。原因：演化循环是"外层驱动器"，不是 App 内部组件，不该污染 AppBuilder。

## 2. 现有可复用资产清单（不重造）

| 闭环段 | 现有模块 | file:line | 复用方式 |
|---|---|---|---|
| ① 轨迹 | `RecordingProvider` | `crates/oneai-eval/src/replay.rs:148` | 包住真实 provider，录 `Trajectory` |
| ① 轨迹 | `InMemoryCollector` + `TraceTree` | `crates/oneai-trace/src/collector.rs:43` `tree.rs:65` | 跨度树供诊断遍历 |
| ① 轨迹 | `TraceEvent`/`EventKind`/`Span`/`spans_by_kind` | `event.rs:26,101` `span.rs:73` | 子图提取的原料 |
| ② 评分 | `EvalRunner.run(suite)` | `eval_runner.rs:104,136` | 直接当 EDD 评分器，**不改** |
| ② 评分 | `EvalResult`(scores/trace_metrics/efficiency/usage) | `eval_result.rs:21` | 低分筛选的输入 |
| ② 评分 | `EfficiencyProfile` | `efficiency.rs:27` | 多目标轴(tokens/latency/iterations) |
| ② 评分 | trace-aware metric: `TrajectoryMetric`/`EfficiencyMetric`/`LlmJudgeMetric`/`CompositeMetric` | `builtin_metrics.rs:278,810,455,723` | 客观 + judge 双轨 |
| ② 多目标 | SWE-bench 三轴 (capability×cost×efficiency) | `swebench/runner.rs:68` | Pareto 目标基底 |
| ④ 变异基质 | `DomainPackConfig` (string/Vec/HashMap) | `config_parser.rs` (经 `spec_file.rs:107`) | LLM 可直接改写 |
| ④ 校验 | `DomainPackSpecFile::validate_and_build` | `spec_file.rs:107,115` | 变异后校验→构建，零重编 |
| ④ 加载 | `AppBuilder.domain_pack(pack: DomainPack)` | `builder.rs:630` | 每候选起独立 App |
| ④ lesson 基质 | `SkillMetadataStore` + `SkillCurator` + `run_consolidation` | `oneai-skill/curator.rs` `lifecycle.rs` | skill 维度已有"变异+合并+选择"，可借形 |
| 收敛 | `TokenBudget` + `budget.remaining()` | `oneai-core/budget.rs` | 演化代数上限 |
| 安全 | `PermissionResolver` + `DomainPackValidator` | `oneai-core`/`validator.rs` | 变异产物落地前必过 |

## 3. 新增模块设计

### 3.0 变异基质全图（CandidateConfig 可寻址字段）

变异只在 **spec 空间**（`DomainPackConfig` + `AgentLoopConfig` 可变异旋钮 + skill 文本）。下表是全部纳入的轴、变异类型、杠杆与 hack 面、批次。`DomainPackConfig` 全 serde-able（string/Vec<String>/HashMap/枚举名/数值），LLM 可直接改写，经 `DomainPackSpecFile::validate_and_build()` 校验热加载，**零重编 Rust**。

| 轴 | DomainPackConfig 字段 / AgentLoopOverlay | 变异类型 | 杠杆 | hack 面 | 批次 |
|---|---|---|---|---|---|
| system_prompt | `system_prompt` | 自由文本 | 高 | 高 | E3 首选 |
| tool 描述 | `tool_decorators: HashMap<String,String>` | 自由文本 | 高（左右选工具） | 高 | E3 首选 |
| tool 集 | `tools: Vec<String>` | 离散集合（加/减） | 高（"没必要用"/"缺能力"） | 低 | E3 首选 |
| 压缩保留 | `compression_template.preserve_fields` + `truncate_rules` | 混合 | 中（压缩信息丢失） | 中 | E3 |
| 记忆抽取 | `memory_profile.extraction_schema` | 字符串类别表 | 高（压缩→LTM 抽什么） | 中 | E0 spec 化 + E3 后期 |
| 记忆召回 | `memory_profile.recall`(strategy/top_k/time_decay) | 枚举+数值 | 高（长任务召回对不对） | 低 | E0 + E3 后期 |
| 记忆衰减 | `memory_profile.decay`(enabled/阈值/ttl/half_life) | 枚举+数值 | 中（陈旧噪声） | 低 | E0 + E3 后期 |
| 工作态压缩 | `memory_profile.working_state.compaction` + retention | 枚举+数值 | 中（长任务崩溃恢复） | 低 | E0 + E3 后期 |
| 环境注入 | `context_sources: Vec<String>` | 离散集合 | 中（注入哪些环境信息） | 低 | E3 次 |
| 范式触发 | `paradigm_strategies[i].trigger_pattern` | 正则/字符串 | 中（"规划流程不合理"） | 中 | E3 后期 |
| 权限集 | `permission_profile`(auto_approve/require/deny) | 离散集合 | **低（headless eval 无信号）** | — | E5 慎/低优先 |
| 采样/预算 | `AgentLoopOverlay`(temp/top_p/thinking_budget/hard_max_iterations/token_budget) | 数值 | 中（temp/top_p 见下注） | 中 | E3 次 |
| skill 文本 | skill_overrides | 自由文本 | 中 | 中 | E3 次（借 SkillCurator） |

**批次判定理由**：
- **E3 首选 = system_prompt + tool_decorators + tools**：用户最初诊断目标"哪些工具没必要用、哪些用错了"逐字命中 tool_decorators/tools；system_prompt 是最高杠杆自由文本。三者构成"工具选择问题"的完整三角。
- **MemoryProfile 高杠杆但需长 horizon case suite**：extraction_schema/recall/decay 的效果在短期 case 看不出（top_k=5 vs 8 在 3 轮任务里无差别），必须 50+ 轮的长任务用例才显现。故 E0 先修 spec 通路、E3 后期接轴。
- **permission_profile 低优先**：headless eval 用 `noop_interaction_gate`（`eval_runner.rs` 路径），权限变更对评分零信号——只在真实交互场景才有意义，故推迟到 E5 且低优先。
- **temperature 在 eval 循环里冻结为 0**：eval 优化的是确定性策略；若要调 temperature 必须多采样平均（贵），首版不做。top_p 同理。故 AgentLoopOverlay 实际变异的是 thinking_budget / hard_max_iterations / token_budget，temp/top_p 固定。

**安全闸（已存在，复用不新造）**：
- `DomainPackValidator` 已查未知工具名（`validator.rs:352` `known_tool_names()`）→ `tools` 减/加变异落地前必过。
- `PermissionResolver` 三路径（phase1-1p4）→ permission 变异落地前必过。
- `DomainPackSpecFile::validate_and_build()` → 所有 spec 变异统一入口。

### 3.1 `oneai-evolve` crate 骨架

```
crates/oneai-evolve/src/
  lib.rs
  candidate.rs        // CandidateConfig: DomainPackConfig + AgentLoopConfig delta + skill overrides
  trajectory_collector.rs  // ① 录 Trajectory + TraceTree (薄包衣)
  failure_extractor.rs     // ② 低分筛选 (极薄)
  subgraph.rs               // ③ Minimal Subgraph 诊断 (核心)
  gepa.rs                   // ④ 变异+Pareto+lesson 合并 (核心)
  lessons.rs                // 跨代 LessonsLog (持久化)
  loop.rs                   // EvolutionLoop 驱动器 (⑤ 收敛)
  cli.rs                    // oneai evolve run/step/report/diff 子命令
  tests/                    // e2e 用 mock provider 跑全闭环
```

依赖：`oneai-app oneai-eval oneai-domain oneai-trace oneai-skill oneai-core`。零新外部依赖。

### 3.2 关键类型与 trait（草签）

```rust
// candidate.rs —— 变异单元 = 一份可热加载的配置
#[non_exhaustive]
pub struct CandidateConfig {
    pub pack_config: oneai_domain::DomainPackConfig,   // 主变异基质
    pub loop_overlay: AgentLoopOverlay,                 // system_prompt/temp/top_p/thinking_budget/...
    pub skill_overrides: Vec<SkillOverride>,            // skill 文本 patch
}
#[non_exhaustive]
#[derive(Clone)]
pub struct AgentLoopOverlay {
    pub system_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub thinking_budget: Option<u32>,
    pub hard_max_iterations: Option<usize>,
    pub token_budget: Option<u32>,
    // 只列"可变异"旋钮; 其余继承 baseline
}
impl CandidateConfig {
    /// 校验 → 构建 DomainPack → 起 App。
    pub fn build_app(&self, baseline: &AppBaseline, project_dir: &str)
        -> std::result::Result<oneai_app::App, ValidationError>;
}

// subgraph.rs —— ③ 诊断
#[non_exhaustive]
pub struct Diagnosis {
    pub suspect_params: Vec<ParamRef>,    // e.g. ("pack.system_prompt"), ("pack.compression.preserve_fields[2]")
    pub subtrace: TraceSlice,             // 裁剪后的最小因果 span 子树
    pub critique: String,                 // LLM-judge 自然语言归因
}
#[non_exhaustive]
pub enum ParamRef {                       // 指向 CandidateConfig 的可寻址字段(见 §3.0 全图)
    // ── E3 首选轴 ───────────────────────────────
    PackSystemPrompt,
    PackToolDecorator(String),              // 工具名 → 描述覆盖(spec 唯一通道)
    PackTool(String),                       // 工具集 加/减
    // ── E3 次轴 ───────────────────────────────
    PackCompressionField(usize),
    PackContextSource(String),               // 环境注入 加/减
    PackParadigmTrigger(usize),             // 范式触发 pattern
    LoopThinkingBudget,
    LoopHardMaxIterations,
    LoopTokenBudget,
    // ── E0 spec化 + E3后期 ────────────────────
    PackExtractionSchema,                   // memory_profile.extraction_schema
    PackRecallTopK,                          // memory_profile.recall.top_k/strategy
    PackDecay(DecayField),                   // memory_profile.decay.*
    PackWorkingStateCompaction(CompactionField),
    // ── E5 慎 ─────────────────────────────────
    PackPermission(String),                 // permission_profile.* (低优先, headless无信号)
    SkillText(String),                       // skill 文本 patch
}
pub trait SubgraphDiagnostician: Send + Sync {
    /// 在 trace_tree 上提取连接 suspect 参数与失败输出的最小子树,
    /// 再让 LLM-judge 读子树给 critique + 候选 suspect_params.
    async fn diagnose(&self, fc: &FailedCase, cfg: &CandidateConfig)
        -> Diagnosis;
}

// gepa.rs —— ④ 变异+Pareto+lesson
#[non_exhaustive]
pub struct GepaConfig {
    pub population: usize,            // 每代候选数 K (默认 4)
    pub max_generations: usize,       // 默认 8
    pub target_pass_rate: f64,        // 收敛目标
    pub case_subset_ratio: f64,       // 变异评估用 case 子集比例 (省钱, 默认 0.4)
    pub judge_model: ModelConfig,     // LLM-as-judge 用的(可更强的)模型
}
pub trait VariationOperator: Send + Sync {
    /// 读 Diagnosis, 产出 K 个候选 patch (只动 suspect_params 指向的字段).
    async fn vary(&self, d: &Diagnosis, base: &CandidateConfig, k: usize)
        -> Vec<CandidateConfig>;
}
pub trait ParetoSelector: Send + Sync {
    /// 多目标非支配排序: (pass_rate↑, tokens↓, latency↓).
    fn select(&self, scored: &[ScoredCandidate], k: usize) -> Vec<ScoredCandidate>;
}
pub trait LessonMerger: Send + Sync {
    /// GEPA 核心: 取 Pareto 前沿互补 lesson 拼下一代 base.
    async fn merge(&self, frontier: &[ScoredCandidate]) -> CandidateConfig;
}

// loop.rs —— ⑤ 驱动
pub struct EvolutionLoop {
    pub baseline: AppBaseline,         // provider + project_dir + suite + metrics
    pub diagnostician: Arc<dyn SubgraphDiagnostician>,
    pub optimizer: GepaOptimizer,      // operator + selector + merger
    pub config: GepaConfig,
    pub lessons: LessonsLog,
}
impl EvolutionLoop {
    pub async fn run(&mut self, seed: CandidateConfig) -> EvolutionReport;
}
```

### 3.3 三个真正"新"的难点（其余都是粘合）

**难点 A — Minimal Subgraph 子图提取（subgraph.rs）**：Trace 是层级 span 树 + 扁平 event，不是 Trace(MSR) 那种显式计算图。做法：
1. 把 CandidateConfig 的每个可变异参数注册一个 `ParamRef`，并记录它"影响哪些 span"——例如 `PackSystemPrompt` 影响所有 `SpanKind::LLM` 跨度；`PackToolDecorator(name)` 影响该 tool 的 `ToolCall`/`ToolResult` span；`PackCompressionField(i)` 影响压缩触发后的 `ParseFallback`/`WorkflowStep` span。
2. 对一个 FailedCase，从"失败输出"span（最后一个 `Thought`/`Action` 误差）反向 BFS，只保留路径上含"被影响 span"的最小子树。
3. 把这个子树 + suspect_params 喂 LLM-judge，产出 critique。
   - 这一步是 Trace(MSP) 的"传播 Minimal Subgraph"在 OneAI 上的实体化：用 span 因果邻接代替梯度。
   - 兜底：若子图提取失败（参数影响映射不全），退化为"取最后 N 轮 Thought/Action/Observation"喂 judge——即 Reflexion 式全轨迹反思。保证不卡死。

**难点 B — Pareto 多目标排序（gepa.rs）**：直接用 SWE-bench 三轴 `(pass_rate, total_tokens, latency_ms)`。`pass_rate` 来自 `EvalReport.summary`，`total_tokens`/`latency_ms` 来自 `EfficiencyProfile`。非支配排序后取前沿，`LessonMerger` 把前沿候选的"各自最优维度"patch 拼到下一代 base。这正是 GEPA 的"complementary lessons from the Pareto frontier"。

**难点 C — 变异评估的 token 成本**：每代 K 个候选 × 全 suite 跑 live LLM 太贵。两招省钱 + 一道回归闸：
1. **case 子集（变异评估）+ 全 suite（收敛判定）**：变异评估只跑 `case_subset_ratio` 比例的 case（首尾 + 失败 case 优先），收敛判定时才跑全 suite。这是 GEPA "35x fewer rollouts" 的实质——分级采样而非少采样。
2. **预算硬顶**：`budget.remaining() < 阈值` 即停，复用 `TokenBudget` 机制 + 早停（连续 2 代 frontier 无提升即停，防震荡）。
3. **Trajectory replay 作回归闸，不作变异评估**（修正先前表述）：`ReplayProvider` 重放的是冻结响应，**任何改变模型输出的变异（system_prompt/temp/thinking_budget/tools/decorators/recall/compression）都会让冻结响应失效，replay 给出的是假画面**。故 replay **只用于回归**——对最终 frontier config 跑 `replay_trajectory`（replay.rs:247）确认其行为仍确定、未引入非确定性漂移；**不**用于省变异评估的 token。唯一 replay 能安全估的变异是"只改 hard_max_iterations/token_budget 这类纯截断闸"且原轨迹本就提前结束——价值边际，首版不做。

> **设计取舍**：首版省钱只靠 (1) case 子集 + (2) 预算硬顶。replay 退为回归闸。这是诚实的——先验里没有任何系统靠 replay 评估变异后的 agent，变异后必须 live 重跑。

## 4. 分阶段实施（Phase E1–E5）

每个 Phase 独立可测、可提交、有绿测 + fmt + clippy(-D) + deny。

### Phase E0 — MemoryProfile spec 化（前置必修）
**目标**：让 `MemoryProfile` 进入 `DomainPackConfig` 可序列化形态，与 `PermissionProfileConfig`/`CompressionTemplateConfig` 平级，使变异→校验→热加载通路对记忆策略成立。
**背景**：当前 `DomainPackConfig`（config_parser.rs:93）**无 `memory_profile` 字段**——`DomainPackSpecFile::validate_and_build()` 无法 round-trip MemoryProfile，变异了无处落地。这是把记忆策略纳入自演进的硬阻塞。
- 新增 `MemoryProfileConfig`（config_parser.rs）：全 serde-able —— `extraction_schema: Vec<String>`、`recall_strategy: String`(枚举名)、`recall_top_k: usize`、`recall_time_decay: bool`、`core_budget_tokens: usize`、`enable_memory_tools: bool`、`habit_fact_types: Vec<String>`、`decay: DecayPolicyConfig`、`working_state: WorkingStatePolicyConfig`、`skill_lifecycle: SkillLifecyclePolicyConfig`。全是字符串/枚举名/数值——LLM 可改，且 hack 面比自由文本小。
- `DomainPackConfig` 加 `#[serde(default)] pub memory_profile: MemoryProfileConfig`。
- `resolve_config` 加 `MemoryProfileConfig → MemoryProfile` 转换（建 `FactType::new`、`RecallStrategy::parse`、`DecayPolicy`/`WorkingStatePolicy`/`SkillLifecyclePolicy` from config）。
- `DomainPackValidator` 加语义校验：`decay.min_salience∈[0,1]`、`archive_forget_salience≤min_salience`、`core_budget_tokens≤`合理上界、`skill_lifecycle.stale_after<archive_after`、`compaction.keep_recent<event_threshold`。
- `CodingPack`/`ResearchPack` 的 MemoryProfile 能经 spec 往返（round-trip 测试）。
- **测试**：spec → validate → build → 与 `MemoryProfile::coding()` 字段级相等；非法 decay 值被 validator 拒。绿。
- **不改 E1+ 逻辑**：纯 schema 扩展，向后兼容（`#[serde(default)]`）。

### Phase E1 — 闭环骨架 + EDD 接线（无优化，先通水管） ✅ 已落地
**目标**：`oneai evolve run --seed <pack> --suite <s>` 能跑出"baseline 评分 + 失败 case + 其 Trajectory/TraceTree"的报告，不做任何变异。
- 新建 `oneai-evolve` crate（lib + workspace 注册）。✅
- `CandidateConfig` + `build_app()`：经 `DomainPackSpecFile::from_config(cfg).validate_and_build(project_dir)` → `AppBuilder.domain_pack(pack)`。验证 seed pack 能热加载。✅
- `TrajectoryCollector`：包 `RecordingProvider` + `trace_in_memory()`，每 case 产出 `(Trajectory, TraceTree)`。✅（复刻 `EvalRunner::run_agent_for_case`，补 EvalRunner 丢弃的 per-case Trajectory/TraceTree；`recorded_tool_calls` 从 `SpanKind::TOOL` 的 `tool.name` attr 真取）
- `EvolutionLoop.run()` 退化版：只跑①②，`EvolutionReport` 含 per-case EvalResult + Trajectory 落盘（`<root>/evolve/run-<ts>/case-<id>.jsonl`）。✅
- CLI：`oneai evolve run --seed --suite --no-optimize`。✅
- **测试**：mock provider + CodingPack seed + 3 case suite → 报告含 3 个 EvalResult + 3 个 Trajectory 文件。绿。✅（`tests/e2e_e1.rs`，3 unit + 1 e2e 绿）
- **不做**：诊断、变异、Pareto。✅

### Phase E2 — Minimal Subgraph 诊断 ✅ 已落地
**目标**：对每个 FailedCase 输出 `Diagnosis{suspect_params, subtrace, critique}`。
- `ParamRef` 枚举 + "参数→影响 span 映射"注册表（`subgraph.rs`）。✅
- 子图提取：反向 BFS + 影响映射裁剪；兜底退化为尾 N 轮。✅（`failure_span` = 最后一个 `LLM` span 的 ancestry；`suspect = affected_spans ∩ failure_path`；空集退化为 tail-N 轮 + 全候选 suspect）
- `SubgraphDiagnostician` trait + 默认实现 `LlmDiagnostician`（用 `LlmJudgeMetric` 已有的 judge provider，对齐 `builtin_metrics.rs:455`）。✅（`HeuristicDiagnostician` 确定性默认 + `LlmDiagnostician` 复用同形 judge 调用只重写 critique；judge 无则退化启发式）
- 诊断结果落盘 + 在 report 里渲染。✅（`run-<ts>/diagnosis-<id>.json` + `EvolutionReport.diagnoses` 内联 summary）
- **测试**：构造一个"system_prompt 缺关键指引 → 模型不调工具 → 失败"的 case，断言 `Diagnosis.suspect_params` 含 `PackSystemPrompt`。绿。✅（`tests/e2e_e2.rs`：错答 case → suspect 含 PackSystemPrompt、tools 非 suspect、diagnosis 文件 round-trip；+全通过零诊断、+trait 覆盖 seam 三测；subgraph.rs 5 unit）

> **实现取舍**（与设计稿微调，记入 memory）：默认 diagnostician 用 `HeuristicDiagnostician`（纯启发式、确定性、无需 LLM），而非设计稿的"`LlmDiagnostician` 默认"。理由：eval 测必须确定性；judge provider 在 E2 测试阶段未注入。`LlmDiagnostician` 仍提供，复用启发式核心的 `suspect_params`+`subtrace`，仅用 judge 重写 `critique` 文本——E5 注入更强/异家 judge。`Diagnosis::new` 构造器因 `#[non_exhaustive]` 提供。

### Phase E3 — GEPA 变异 + Pareto 选择
**目标**：单代"变异 K → 评分 → Pareto 选前沿"，首批轴 = **工具三件套（system_prompt + tool_decorators + tools）**，次轴 = compression/context/thinking_budget。
- `VariationOperator` + 默认 `LlmVariationOperator`：读 Diagnosis + 当前 config，让 LLM 产 K 个 patch（**首批只动 `ParamRef::{PackSystemPrompt, PackToolDecorator, PackTool}`**），每个 patch 经 `validate_and_build` 过校验，不过的丢弃并 log（validator 已查未知工具名 validator.rs:352）。
- **reward-hacking 防护（首批轴尤其需要）**：
  - 变异评估用 **训练 case 子集**，收敛判定用**留出 held-out case**——否则 LLM 可把工具描述写成"任务 X 就调我"拟合训练集。
  - tool_decorators 变异加轻量语义校验：装饰器描述**不得与工具 input schema 自相矛盾**（如把 read_file 描述成写文件）——低成本防"描述作弊到破坏语义"。
  - judge_model 与 candidate model 分离（TextGrad 双 LLM 非对称），防自评偏置。
- 候选评分：`case_subset_ratio` 子集 + live LLM（**所有首批轴都是语义变异，必须 live，不可 replay**——见难点 C）。
- `ParetoSelector` 默认实现：三轴非支配排序（pass_rate↑, tokens↓, latency↓）。
- 单代 `step()`：seed → diagnose → vary → score → select。
- **测试**：mock provider + 一个"system_prompt 不够好 / 工具描述误导选错工具"的 seed → 一代后 frontier 的 pass_rate ≥ seed；构造一个"装饰器描述与 schema 矛盾"的负测断言被拒。绿（mock 下确定性）。

### Phase E4 — lesson 合并 + 跨代记忆 + 收敛 + MemoryProfile 轴接入 ✅ 已落地
**目标**：多代闭环跑到收敛或 max_generations；MemoryProfile 轴（extraction_schema/recall/decay）在长 horizon case suite 上接入。
- `LessonMerger` 默认实现：`BestFrontierMerger`（前沿最优=下一代 base；互补 lesson 拼接的 seam 留给更丰富实现，mock 下不可确定测试故首版取前沿最优）。✅（`lessons.rs`，trait + `BestFrontierMerger`）
- `LessonsLog`：跨代持久化（`<run_dir>/lessons.jsonl`），每代记 `(generation, base_pass_rate, frontier_pass_rate, frontier_axes, is_seed, lessons_text)`。✅
- 收敛判定：`frontier_pass_rate ≥ target` OR `max_generations` OR `max_total_tokens` 预算硬顶。✅
- 早停：连续 `early_stop_patience`（默认2）代 frontier 无提升 → 停（防震荡）。✅（`LessonsLog::gens_without_improvement`）
- **MemoryProfile 轴接入**：`ParamRef::PackRecallTopK`（`pack.memory.recall.top_k`，Set+界检查 [1,128]）+ `PackExtractionSchema`（Add/Remove）经 `from_path`/`apply_patch` 落地，经 `validate_and_build` 校验。✅
- **实现取舍**（与设计稿微调，记入 memory）：
  - 收敛判定用 **subset** frontier-best pass_rate（held-out 全 suite 收敛闸是 E5，非 E4——保持 E3 单代成本不变）。
  - `LessonMerger` 默认取前沿最优而非"互补 lesson 拼接"——拼接需 patch provenance（`ScoredCandidate` 只带完整 config 不带 patch 列表），且 mock 下不可确定测试。trait 是 seam，E5+ 可换更丰富 stitcher。
  - `GepaConfig` 新增 `max_total_tokens: Option<u64>` + `early_stop_patience: usize`（设计 §3.2 的 budget/patience 落点）。
  - 长 horizon suite 的"召回集行为变化"需 live embedding-backed memory，属 E5 live smoke；E4 在 config 级断言 `recall.top_k` 端到端流转（patch→validate→persist round-trip）。
- **测试**：mock 下跑 3 代，断言每代 frontier pass_rate 单调不降 + `lessons.jsonl` 3 行落盘 + `stop_reason="max_generations"`；`recall.top_k` patch 端到端到持久化 frontier config。绿（`tests/e2e_e4.rs` 2 e2e + gepa 5 unit MemoryProfile 轴）。

### Phase E5 — CLI 打磨 + 安全护栏 + 回归闸 + 文档
- CLI 全套：`oneai evolve run/step/report/diff/lesson`（diff = 当代 best vs seed 的 config diff）。
- 安全护栏：
  - 变异产物落地前必过 `DomainPackValidator` + `PermissionResolver` 三路径（对齐 phase1-1p4）；校验不过的候选直接丢弃并 log。
  - 变异**不自动改 Rust 代码**；若诊断指向"需改代码/加工具"，输出"建议工单"到 report 而非自动执行。
  - judge_model 与 candidate model 分离。
  - **回归闸**：最终 frontier config 跑 `replay_trajectory` 确认行为确定性未漂移（replay 在此用途，非变异评估）。
  - held-out case 全 suite 跑一次确认 frontier 未过拟合训练子集。
- 文档：`docs/self-evolution-mechanism.md`（机制全文）+ README 章节镜像 EN + memory 条目。
- **测试**：e2e 跑全闭环（mock）+ 一个"变异产出非法 pack"的负测（断言被丢弃不 panic）+ 一个"frontier 过拟合训练集、held-out 反降"的回归测。绿。

## 5. 接线点（要改/加的现有文件，最小侵入）

| 文件 | 改动 | 性质 |
|---|---|---|
| `Cargo.toml`(workspace) | 加 `oneai-evolve` 成员 | 新增 |
| `crates/oneai-evolve/Cargo.toml` | 新建,引用 workspace deps | 新增 |
| `crates/oneai-domain/src/spec_file.rs` | 可能加 `pub fn config_ref(&self) -> &DomainPackConfig`（若未暴露）| 极小 |
| `crates/oneai-domain/src/config_parser.rs` | **E0**: 新增 `MemoryProfileConfig` + `DecayPolicyConfig`/`WorkingStatePolicyConfig`/`SkillLifecyclePolicyConfig` + `DomainPackConfig.memory_profile` 字段 + `resolve_config` 转换 | 中 |
| `crates/oneai-domain/src/validator.rs` | **E0**: 加 MemoryProfile 语义校验；**E3**: 加 tool_decorators 描述-vs-schema 矛盾校验 | 中 |
| `crates/oneai-domain/src/memory_profile.rs` | **E0**: 加 `MemoryProfile::from_config(cfg)` 构造 + 各枚举的 `parse`/`from_str` | 中 |
| `crates/oneai-eval/src/replay.rs` | `Trajectory` 已 pub, 复用; 可能加 `Trajectory::with_trace` 一起存树 | 极小 |
| `crates/oneai-app/src/builder.rs` | **不改**（`domain_pack()` 已满足） | 0 |
| `examples/cli`（crate `oneai-cli`，bin `oneai`） | 加 `evolve` 子命令 | 新增 |
| `README.md` + `README_EN.md` | 加自演进章节（双向同步） | 文档 |

**对 AppBuilder 的态度**：与 studio/supervisor 一致——**不加 AppBuilder 方法**，演化循环是外层驱动器，通过构造 `App` + 直接调 `EvalRunner` 驱动，不污染 App 的组装面。

## 6. 关键设计决策与权衡

1. **为什么是 GEPA 式 reflect 而非 TextGrad 式 backward**：TextGrad 的"textual gradient"需要一个 Variable 在 trace 中可定位反向传播路径；OneAI 的 trace 是 span 树不是计算图，强行套 backward 要造全套图 IR。GEPA 的"采样轨迹→自然语言 reflect→改 prompt→Pareto 合并"与 span 树诊断天然对齐，且 Pareto 多目标直接复用 SWE-bench 三轴。**选 GEPA。** TextGrad 的双 LLM 非对称（judge 用更强模型）借来用。
2. **为什么 skill 维度可借 SkillCurator 形**：`SkillCurator` 的 run/status/pin/archive + `run_consolidation` 已是"变异+合并+选择"的雏形（phase2-1 stage B/C）。E3 的 `VariationOperator` 在 skill 文本维度可直接复用 consolidation 的合并语义；但 pack 维度（system_prompt/compression）是新的，要新写算子。
3. **case 子集 vs 全 suite 的取舍**：变异评估用子集（省钱、快收敛），收敛判定用全 suite（防子集过拟合）。这是 GEPA "35x fewer rollouts" 思路的具体化——不是少采样，而是分级采样。
4. **replay 只对"非语义变异"有效**：明确写进文档——改 prompt/compression 的候选必须 live，replay 只服务 temperature/top_p/thinking_budget/max_iterations 这类数值旋钮的快速估行为漂移。混用会得假阳性。
5. **不收敛的退路**：若连续 N 代无提升，系统输出"当前 frontier + 剩余 suspect_params 清单 + 人工建议"后停，不强行续跑烧 token。
6. **reward-hacking 防护分层**：自由文本轴（system_prompt/tool_decorators）hack 面最高——LLM 可写"任务 X 就调我"拟合特定 case。分层防：①训练/留出 case 分离（变异评估用训练子集，收敛判定用 held-out 全 suite）；②tool_decorators 加"描述不得与工具 input schema 矛盾"的轻量语义校验；③双 LLM 非对称（judge 更强、与 candidate 不同家）。约束枚举/数值轴（MemoryProfile 的 recall/decay/core_budget）天然 hack 面小，无需额外防护。
7. **temperature/top_p 在 eval 循环冻结为 0**：eval 优化的是确定性策略；调 temperature 必须多采样平均才公允（贵），首版不做。故 AgentLoopOverlay 实际可变异旋钮 = thinking_budget / hard_max_iterations / token_budget；temp/top_p 固定。
8. **extraction_schema 是"压缩信息丢失"的另一半旋钮**：FactExtractor 的 LLM prompt 由 `MemoryProfile.extraction_schema` 派生（fact_extraction.rs:5 "guided by the active extraction_schema"），故改 schema 直接改压缩时抽什么事实进 LTM——与 `compression_template.preserve_fields`（压缩时留什么在上下文）是同一故障模式的两个抓手。这是 MemoryProfile 必须纳入基质的技术根据。

## 7. 风险与回退

| 风险 | 缓解 |
|---|---|
| 变异产出语义破坏的 pack（validator 过但行为崩） | 每候选评分时套 `token_budget` 硬顶 + 失败 case 不影响 frontier（直接淘汰） |
| LLM-judge 偏置导致 suspect_params 指错 | ground-truth metric 主导"是否改进"；judge 只做归因；多 judge 投票（≥2/3）才采纳 suspect |
| token 成本失控 | `budget.remaining()` 硬顶 + case 子集 + 早停（replay 已退为回归闸，不省变异 token） |
| 变异收敛到局部最优 | LessonMerger 的"互补 lesson 拼接"专门破局部最优（GEPA 的设计本意）；可加随机重启 |
| 热加载 pack 与运行时缓存不一致 | 每候选起全新 `App`（EvalRunner 已是每 case 新 session），无缓存穿透 |
| 供应链（新依赖） | 零新依赖；现有 workspace deps 全覆盖 |

## 8. 与已有 Phase 的关系

- **不冲突 phase2-1 reflection**：phase2-1 是"任务内 reflect 子代理"，本系统是"跨任务外层演化循环"。reflect 子代理可在 E2 诊断阶段被复用为"LLM-judge"的一个实现，但循环本身不依赖它。
- **不冲突 phase2-1 skill lifecycle**：skill 的变异/合并/选择可由 E3 的 skill 维度算子接手，consolidation 成为它的一个特例。
- **不冲突 SWE-bench 三轴**：直接复用为 Pareto 目标。
- **依赖（已落地，可上建）**：phase1-1p4 的 PermissionResolver/validator（变异产物安全落地）、phase2-1 的 SkillCurator（skill 维度算子）、SWE-bench 三轴（Pareto 目标）。
- **E0 是新前置**：MemoryProfile spec 化（§4 Phase E0）不依赖任何未落地模块，纯 `oneai-domain` 内 schema 扩展，但**必须先于 E1 完成**——否则记忆策略轴无法热加载。

## 9. 验收标准（全部 Phase 完成）

- `oneai evolve run --seed <pack> --suite <s> --target 0.85` 能在 mock provider 下跑通全闭环、产出报告 + lessons.jsonl。
- 报告含：每代 frontier config、三轴 metric、suspect_params 诊断、收敛代数、token 总开销、**训练 vs held-out pass_rate 对比**（防过拟合）。
- 全套测试绿 + fmt + clippy(-D) + deny ok。
- `docs/self-evolution-mechanism.md` + README 双语章节 + memory 条目。
- **不验收**：自动改 Rust 代码、权重微调、全自主无人值守产线部署——这三项明确排除（黑盒 API 不可用 + 供应链戒律 + 先验调研证明产线无可靠案例）。
