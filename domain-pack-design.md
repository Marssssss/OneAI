# 编码 Agent 如何将工作流嵌入框架 — 及 OneAI 的通用化应对

## 第一部分：三个编码 Agent 的"编码工作流嵌入"机制拆解

### 一、共同架构：模型即编排器 + 编码专用工具集

三个框架共享一个核心范式——**LLM 是唯一的编排器**，没有外部脚本或规则引擎决定流程。工作流是通过**工具设计**、**上下文注入**和**权限约束**三重机制"嵌入"到框架中的，而非硬编码逻辑。

```
通用 Agent Loop:
┌───────────────────────────────────────────────┐
│  while (model says "continue") {               │
│    context = assemble(workspace_state + history)│
│    response = model.infer(context + tools)     │
│    if tool_calls: execute → feed back          │
│    else: final answer → break                  │
│  }                                              │
└───────────────────────────────────────────────┘

编码工作流不是"写在外面的脚本"，而是"通过工具描述+上下文+权限
引导模型自然产生编码行为"
```

### 二、编码工作流嵌入的5层机制

#### 第1层：工具集 — 编码操作的精确映射

**Claude Code（13 个工具）**：

| 工具 | 编码操作映射 | 关键设计 |
|------|-------------|---------|
| `Read` | 阅读源代码 | offset+limit 分页读大文件，避免撑爆上下文 |
| `Edit` | 修改代码 | exact string match + uniqueness 约束，防止误改 |
| `Write` | 创建新文件 | 全量覆盖，仅在创建新文件时使用 |
| `Glob` | 发现文件 | 按模式匹配文件路径，比 Grep 快 |
| `Grep` | 搜索代码内容 | ripgrep 正则搜索，支持类型/上下文行 |
| `Bash` | 运行编译/测试 | sandboxed，timeout，description 强制写意图 |
| `Agent` | 委托子任务 | 6种子代理类型，worktree隔离 |
| `NotebookEdit` | 编辑 Jupyter | cell_id 稳定标识，支持 replace/insert/delete |

**OpenCode（13 个工具）**：

| 工具 | 编码操作映射 | 关键设计 |
|------|-------------|---------|
| `read` | 阅读源代码/目录列表 | 同一个工具既可读文件也可列目录 |
| `edit` | 修改代码 | oldString/newString 模式，支持 replaceAll |
| `write` | 创建/覆盖文件 | BOM 保留 |
| `apply_patch` | 多文件批量编辑 | unified diff 格式，原子应用 |
| `glob` | 发现文件 | ripgrep 驱动 |
| `grep` | 搜索代码内容 | ripgrep 驱动，include 过滤 |
| `bash` | 运行命令 | timeout 2min/max 10min，输出截断 1MB |
| `todowrite` | 任务追踪 | 结构化 todo list (content/status/priority) |
| `question` | 中途提问 | 向用户提问（文本/选择列表） |
| `skill` | 加载技能 | SKILL.md 文件注入到上下文 |
| `webfetch` | 获取网页 | HTML→Markdown 转换 |
| `websearch` | 搜索信息 | Exa/Parallel.ai MCP 集成 |

**Codex CLI（8 个工具）**：

| 工具 | 编码操作映射 | 关键设计 |
|------|-------------|---------|
| `shell` | 运行编译/测试 | Docker sandboxed，无网络默认 |
| `apply_patch` | 修改代码 | structured diff（search+replace blocks），原子应用 |
| `read` | 阅读源代码 | 行范围支持 |
| `write` | 创建文件 | 全量覆盖 |
| `grep` | 搜索代码 | 项目范围 |
| `glob` | 发现文件 | 项目范围 |
| `ls` | 列目录 | 递归/扁平 |
| `patch` | 应用 diff | 标准 unified diff 格式 |

**关键洞察**：工具数量不是重点——**每个工具的 description 和 parameter schema 才是工作流嵌入的核心载体**。模型通过工具描述知道"什么时候用 Read 而不是 Glob"、"什么时候用 Edit 而不是 Write"、"什么时候用 Grep 而不是 Bash(grep)"。工具描述就是编码工作流的"隐性编程"。

#### 第2层：上下文注入 — 编码环境的实时感知

三个框架都用不同机制将编码环境信息注入到每一轮推理中：

| 框架 | 机制 | 注入内容 | 刷新频率 |
|------|------|---------|---------|
| **Claude Code** | 系统提示动态组装 | gitStatus（branch/status/commits）+ 环境信息（platform/shell/cwd）+ 可用技能列表 | 每轮 |
| **OpenCode** | Context Epoch 增量更新 | SystemContext.Source（environment/date/instructions/AGENTS.md） | 首次全量→后续仅增量 |
| **Codex CLI** | 初始注入+自然累积 | 项目目录挂载到沙箱 + 工具执行结果自然累积 | 初始 |

**OpenCode 的 Context Epoch 机制最精巧**：

```
首次（baseline）:
  Source "core/environment" → "Working dir: /project, Git branch: main, Platform: linux"
  Source "core/date" → "Today: 2026-06-11"
  Source "core/instructions" → "From AGENTS.md: Always run tests before committing..."

后续（增量更新）:
  Source "core/date" changed → "Update: Today's date is now: 2026-06-12"
  Source "core/environment" unchanged → 无输出（省 token）
  Source "core/instructions" removed → "Removed: instructions from AGENTS.md"
```

这比 Claude Code 的每轮全量注入更高效——只有变化的部分才产生新 token。

#### 第3层：权限与安全 — 编码操作的分级管控

三个框架都为编码操作设计了分级权限：

| 框架 | 分级机制 | 读操作 | 写操作 | 命令执行 |
|------|---------|--------|--------|---------|
| **Claude Code** | 三级：Auto-approve/Confirm/Deny | ✅ 自动 | ⚠️ 需确认 | ⚠️ 需确认+沙箱 |
| **OpenCode** | per-tool permission | ✅ 自动 | ⚠️ 需确认 | ⚠️ 需确认 |
| **Codex CLI** | 三模式：suggest/auto-edit/full-auto | ✅ 三模式都可见 | suggest=只显示 / auto-edit+=自动应用 | suggest=只显示 / auto-edit=需确认 / full-auto=自动执行 |

**Codex 的沙箱设计最激进**：

```
suggest 模式:  模型输出 → 仅展示建议 → 用户手动执行一切
auto-edit 模式: 文件修改自动应用 + Shell 命令需用户确认
full-auto 模式: 一切自动执行 → 必须在 Docker 沙箱中运行！
```

这体现了编码场景特有的安全考量：**文件修改的风险低于 Shell 命令执行**。一个 typo 在代码里可以回滚，但 `rm -rf` 不可逆。

#### 第4层：范式/子代理 — 编码任务的分层委托

| 框架 | 子代理机制 | 编码专用类型 |
|------|-----------|-------------|
| **Claude Code** | Agent tool（6 种类型） | Explore（搜索代码）、Plan（架构规划）、claude（通用编码） |
| **OpenCode** | Agent 配置 + Skill 系统 | Skill.md 定义编码技能（如 duplicate-pr.md, triage.md） |
| **Codex CLI** | 无子代理 | 依赖模型自身分解任务 |

**Claude Code 的子代理体系最完善**：

```
用户: "重构这个模块的认证系统"

主 Agent → Plan 子代理: "分析认证模块，设计重构方案"
           ↓ 返回计划: 5个步骤，关键文件列表
主 Agent → Explore 子代理: "搜索所有使用 auth 的文件"
           ↓ 返回: 12个文件引用
主 Agent → claude 子代理(worktree隔离): "执行步骤1-3的代码修改"
           ↓ 返回: 修改摘要
主 Agent → claude 子代理(worktree隔离): "执行步骤4-5的代码修改"
           ↓ 返回: 修改摘要
主 Agent → 综合结果 → 用户展示
```

#### 第5层：上下文压缩 — 长编码会话的上下文管理

| 框架 | 策略 | 编码特化 |
|------|------|---------|
| **Claude Code** | `/compact` 命令 + harness 自动压缩 | 保留关键决策和文件状态，丢弃逐行内容 |
| **OpenCode** | Compaction（LLM 总结） | 结构化模板：Goal/Progress/Key Decisions/Next Steps/Critical Files |
| **Codex CLI** | 截断 + 总结 | 工具输出截断（stdout/stderr），较早轮次总结 |

**OpenCode 的压缩模板最贴合编码场景**：

```markdown
# Session Summary
## Goal: Refactor authentication module to use JWT
## Constraints: Must maintain backward compatibility
## Progress:
  - ✅ Step 1: Read current auth implementation
  - ✅ Step 2: Design JWT token structure
  - 🔄 Step 3: Implement token generation (in progress)
  - ⏳ Step 4: Update middleware (blocked by Step 3)
## Key Decisions: Using RS256 signing algorithm
## Next Steps: Complete token generation, then middleware
## Critical Files: src/auth/mod.rs, src/middleware/auth.rs
```

这个模板**专门保留了编码最需要的信息**：进度状态、关键决策、关键文件路径。这是编码场景特有的上下文压缩需求——你不能丢掉"哪个文件我正在改"的信息。

---

## 第二部分：编码工作流嵌入的核心抽象模式

从三个框架的分析中，可以提取出5个**可通用化的抽象模式**：

### 模式1：工具 ≈ 领域操作的精确映射

编码 Agent 的工具不是"通用操作"（如通用的 HTTP 调用），而是**领域操作的精确映射**：

```
编码领域:           Read ≈ 阅读代码 → Edit ≈ 修改代码 → Grep ≈ 搜索代码 → Bash ≈ 运行编译
```

对于通用框架，这个映射应该是**可插拔的领域工具包（Domain ToolPack）**：

```
编码领域 ToolPack:  read_code / edit_code / grep_code / run_tests / ...
研究领域 ToolPack:  web_search / web_fetch / pdf_read / data_extract / ...
数据分析 ToolPack:  query_database / plot_chart / statistical_test / ...
IoT控制 ToolPack:   device_status / send_command / read_sensor / camera_capture / ...
```

### 模式2：上下文 ≈ 领域环境的实时感知

编码 Agent 注入的是编码环境状态（git branch、文件列表、项目结构）。通用框架需要注入**不同领域的环境状态**：

```
编码环境:  git_status + file_tree + project_config + recent_edits
研究环境:  search_results_cache + citation_database + topic_context
数据分析环境:  database_schema + table_stats + query_history
IoT环境:  device_registry + sensor_readings + network_topology
```

### 模式3：权限 ≈ 领域操作的分级管控

编码 Agent 的分级是"读→写→执行"。通用框架的分级应该**按领域定义**：

```
编码权限:  Read代码(自动) → Edit代码(确认) → Shell执行(沙箱+确认)
研究权限:  Web搜索(自动) → Web获取(确认URL安全) → 文件下载(确认)
数据分析权限:  查询DB(自动) → 修改DB(确认) → 删除记录(高风险)
IoT权限:    读取传感器(自动) → 发送命令(确认) → 执行不可逆操作(高风险+审批)
```

### 模式4：范式 ≈ 领域任务的典型策略

编码 Agent 的范式（Explore/Plan/Edit）映射到编码典型任务。通用框架需要**领域特定的典型策略**：

```
编码:   Explore(搜索代码) → Plan(设计重构) → Edit(实现修改) → Test(验证结果)
研究:   Search(搜索文献) → Extract(提取信息) → Synthesize(综合分析) → Verify(验证来源)
数据分析: Query(查询数据) → Transform(转换清洗) → Visualize(可视化) → Interpret(解读)
```

### 模式5：压缩 ≈ 领域关键信息的保留优先级

编码 Agent 的压缩模板保留"关键文件、进度状态、关键决策"。通用框架需要**领域特定的保留优先级**：

```
编码:  关键文件路径 > 进度状态 > 关键决策 > 代码片段
研究:  来源URL > 关键发现 > 论证逻辑 > 原始数据
数据分析: 查询结果 > 分析结论 > 数据特征 > SQL语句
```

---

## 第三部分：OneAI 的通用化架构设计方案

### 核心洞察：编码 Agent 的5层机制不是"编码特有"的，而是"领域特有"的可插拔配置

编码 Agent 把编码工作流嵌入框架的方式，本质是：

> **领域知识 = 工具集描述 + 上下文注入规则 + 权限分级配置 + 范式策略选择 + 上下文压缩优先级**

这5层都是**配置项**而非**硬编码**。Claude Code 把编码的配置硬编码了，但 OneAI 应该把它们做成**可插拔的领域配置包**。

### 架构设计：Domain Pack

```rust
/// 一个领域配置包——包含特定领域（编码、研究、数据分析等）的完整工作流嵌入配置
pub struct DomainPack {
    name: String,                           // "coding", "research", "data_analysis"
    description: String,

    // 第1层: 领域工具集
    tools: Vec<Arc<dyn Tool>>,              // 领域特定的工具注册表

    // 第2层: 领域上下文注入
    context_sources: Vec<ContextSource>,     // 领域特定的上下文源

    // 第3层: 领域权限配置
    permission_profile: PermissionProfile,   // 领域特定的权限分级

    // 第4层: 领域范式策略
    paradigm_strategies: Vec<ParadigmStrategy>,  // 领域特定的范式映射

    // 第5层: 领域压缩优先级
    compression_template: CompressionTemplate,    // 领域特定的上下文压缩模板

    // 领域系统提示模板
    system_prompt_template: String,          // 领域特定的系统提示框架
}

/// 上下文源——可独立刷新的环境信息提供者
pub trait ContextSource: Send + Sync {
    fn key(&self) -> &str;                   // "git_status", "file_tree", "device_registry"
    fn load(&self) -> Result<String>;        // 加载当前状态
    fn refresh_interval(&self) -> Option<Duration>;  // None = 手动刷新, Some = 自动刷新间隔
}

/// 权限配置——领域操作的分级规则
pub struct PermissionProfile {
    auto_approve: Vec<String>,               // ["read_code", "grep_code", "query_db"]
    require_confirmation: Vec<String>,        // ["edit_code", "write_file", "send_command"]
    deny_by_default: Vec<String>,             // ["shell(rm*)", "shell(format*)"]
    risk_overrides: HashMap<String, RiskLevel>, // 工具→风险级别覆盖
}

/// 范式策略——领域任务到 Agent 范式的映射
pub struct ParadigmStrategy {
    trigger_pattern: String,                 // "refactor|modify|implement" → Plan+ReAct
    paradigms: Vec<ParadigmSequence>,        // [Plan, ReAct, Reflection] 或 [Search, Synthesize]
    subagent_types: Vec<SubAgentType>,       // 领域特有的子代理类型定义
}

/// 压缩模板——领域关键信息的保留规则
pub struct CompressionTemplate {
    preserve_fields: Vec<String>,            // ["critical_files", "progress_status", "key_decisions"]
    template: String,                        // 结构化压缩模板
    truncate_rules: HashMap<String, usize>,  // "tool_output" → 2000 chars
}
```

### 具体的 Domain Pack 示例

#### CodingPack（对标 Claude Code）

```rust
pub fn coding_pack(project_dir: &str) -> DomainPack {
    DomainPack {
        name: "coding",
        tools: vec![
            Arc::new(ReadCodeTool::new(project_dir)),     // offset+limit 分页读
            Arc::new(EditCodeTool::new()),                 // exact string match
            Arc::new(WriteCodeTool::new()),                // 创建新文件
            Arc::new(GrepCodeTool::new(project_dir)),     // ripgrep 内容搜索
            Arc::new(GlobCodeTool::new(project_dir)),     // 文件发现
            Arc::new(ShellTool::new_with_filter()),        // 命令执行+黑名单过滤
            Arc::new(RunTestsTool::new()),                 // 编译+运行测试
            Arc::new(ApplyPatchTool::new()),               // unified diff 批量编辑
        ],
        context_sources: vec![
            GitStatusSource::new(project_dir),              // git branch/status/commits
            FileTreeSource::new(project_dir),               // 项目文件结构
            ProjectConfigSource::new(project_dir),         // Cargo.toml/package.json
            RecentEditsSource::new(),                       // 最近修改的文件列表
        ],
        permission_profile: PermissionProfile {
            auto_approve: vec!["read_code", "grep_code", "glob_code"],
            require_confirmation: vec!["edit_code", "write_code", "shell"],
            deny_by_default: vec!["shell(rm*)", "shell(del*)", "shell(format*)"],
            risk_overrides: HashMap::new(),
        },
        paradigm_strategies: vec![
            ParadigmStrategy {  // 重构任务
                trigger_pattern: "refactor|rewrite|restructure",
                paradigms: vec![Plan, ReAct, Reflection],
                subagent_types: vec![Explore, Plan, Code],
            },
            ParadigmStrategy {  // 搜索理解任务
                trigger_pattern: "understand|explain|find|search",
                paradigms: vec![ReAct],
                subagent_types: vec![Explore],
            },
        ],
        compression_template: CompressionTemplate {
            preserve_fields: vec!["critical_files", "progress_status", "key_decisions", "next_steps"],
            template: CODING_COMPRESSION_TEMPLATE,
            truncate_rules: HashMap::from([
                ("tool_output", 2000),  // Shell 输出截断到 2000 chars
                ("file_content", 5000), // 文件内容截断
            ]),
        },
        system_prompt_template: CODING_SYSTEM_PROMPT,
    }
}
```

#### ResearchPack（对标 Perplexity / Deep Research）

```rust
pub fn research_pack() -> DomainPack {
    DomainPack {
        name: "research",
        tools: vec![
            Arc::new(WebSearchTool::new()),                 // 搜索引擎
            Arc::new(WebFetchTool::new()),                  // 网页获取→Markdown
            Arc::new(PdfReadTool::new()),                   // PDF 文档解析
            Arc::new(CitationExtractTool::new()),           // 引用提取
            Arc::new(SummarizeTool::new()),                 // 文章摘要
            Arc::new(CompareSourcesTool::new()),            // 多来源交叉验证
            Arc::new(NoteWriteTool::new()),                 // 笔记记录
        ],
        context_sources: vec![
            DateSource::new(),                               // 当前日期（研究需要时间信息）
            TopicContextSource::new(),                       // 话题背景知识
            CitationCacheSource::new(),                      // 已收集的引用缓存
        ],
        permission_profile: PermissionProfile {
            auto_approve: vec!["web_search", "pdf_read", "summarize"],
            require_confirmation: vec!["web_fetch"],         // 需确认URL安全性
            deny_by_default: vec!["shell"],                  // 研究场景不需要 Shell
            risk_overrides: HashMap::new(),
        },
        paradigm_strategies: vec![
            ParadigmStrategy {  // 深度研究
                trigger_pattern: "research|investigate|analyze|compare",
                paradigms: vec![Search, Extract, Synthesize, Verify],
                subagent_types: vec![Searcher, Extractor, Verifier],
            },
        ],
        compression_template: CompressionTemplate {
            preserve_fields: vec!["source_urls", "key_findings", "arguments", "citations"],
            template: RESEARCH_COMPRESSION_TEMPLATE,
            truncate_rules: HashMap::from([
                ("web_content", 3000),
                ("pdf_content", 5000),
            ]),
        },
        system_prompt_template: RESEARCH_SYSTEM_PROMPT,
    }
}
```

#### DataAnalysisPack

```rust
pub fn data_analysis_pack(db_config: &DatabaseConfig) -> DomainPack {
    DomainPack {
        name: "data_analysis",
        tools: vec![
            Arc::new(QueryDatabaseTool::new(db_config)),    // SQL 查询
            Arc::new(PlotChartTool::new()),                 // 数据可视化
            Arc::new(StatisticalTestTool::new()),           // 统计检验
            Arc::new(DataTransformTool::new()),             // 数据清洗转换
            Arc::new(ExportDataTool::new()),                 // 导出结果
            Arc::new(ReadCodeTool::new(".")),               // 读分析脚本
            Arc::new(WriteCodeTool::new()),                 // 写分析脚本
            Arc::new(ShellTool::new_with_filter()),         // 运行 Python/R 脚本
        ],
        context_sources: vec![
            DatabaseSchemaSource::new(db_config),            // 数据库 schema
            TableStatsSource::new(db_config),                // 表统计信息
            QueryHistorySource::new(),                       // 历史查询缓存
        ],
        permission_profile: PermissionProfile {
            auto_approve: vec!["query_database", "plot_chart", "statistical_test"],
            require_confirmation: vec!["data_transform", "write_code"],
            deny_by_default: vec!["shell(rm*)", "shell(drop*)"],
            risk_overrides: HashMap::from([
                ("query_database", RiskLevel::Low),          // 只读查询=低风险
                ("data_transform", RiskLevel::Medium),       // 数据转换=中风险
            ]),
        },
        paradigm_strategies: vec![
            ParadigmStrategy {  // 数据分析
                trigger_pattern: "analyze|visualize|compare|trend",
                paradigms: vec![Query, Transform, Visualize, Interpret],
                subagent_types: vec![QueryAgent, Analyst, Visualizer],
            },
        ],
        compression_template: CompressionTemplate {
            preserve_fields: vec!["query_results", "analysis_conclusions", "data_characteristics", "sql_statements"],
            template: DATA_ANALYSIS_COMPRESSION_TEMPLATE,
            truncate_rules: HashMap::from([
                ("query_result", 3000),
                ("chart_data", 2000),
            ]),
        },
        system_prompt_template: DATA_ANALYSIS_SYSTEM_PROMPT,
    }
}
```

### Agentic Loop 如何与 Domain Pack 交互

```rust
/// 通用 Agentic Loop — 模型是编排器，Domain Pack 提供领域知识
pub struct AgenticLoop {
    provider: Arc<dyn LlmProvider>,
    domain_pack: DomainPack,           // ← 核心：领域配置包
    memory: Arc<MemoryManager>,
    approval_gate: Arc<dyn ApprovalGate>,
}

impl AgenticLoop {
    pub async fn run(&self, task: &str) -> Result<LoopResult> {
        let mut context = LoopContext::new(task);

        // 初始化：注入领域工具 + 领域上下文 + 领域系统提示
        let tools = self.domain_pack.tools;
        let system_prompt = self.domain_pack.system_prompt_template;
        for source in &self.domain_pack.context_sources {
            let content = source.load()?;
            context.inject_context(source.key(), content);
        }

        loop {
            // 组装推理请求
            let request = self.assemble_request(&context, &tools, &system_prompt);
            let response = self.provider.infer(request).await?;

            match self.classify_response(&response) {
                ResponseType::FinalAnswer => break,
                ResponseType::ToolCalls(calls) => {
                    for call in calls {
                        // 第3层：领域权限检查
                        let permission = self.domain_pack.permission_profile.check(&call);
                        match permission {
                            PermissionAction::AutoApprove => self.execute_tool(call, &mut context),
                            PermissionAction::RequireConfirmation => {
                                let approval = self.approval_gate.request_approval(call).await?;
                                self.execute_with_approval(call, approval, &mut context);
                            }
                            PermissionAction::Deny => context.add_error(call, "Denied by domain permission profile"),
                        }
                    }
                }
                ResponseType::ParadigmSwitch(paradigm_name) => {
                    // 第4层：领域范式策略
                    let strategy = self.domain_pack.find_strategy(&paradigm_name);
                    let result = self.execute_paradigm(strategy, &context).await?;
                    context.merge(result);
                }
                ResponseType::SubTask(task) => {
                    let sub_result = self.spawn_sub_agent(task).await?;
                    context.merge_sub_result(sub_result);
                }
            }

            // 第2层：刷新领域上下文
            for source in &self.domain_pack.context_sources {
                if source.should_refresh() {
                    let content = source.load()?;
                    context.update_context(source.key(), content);
                }
            }

            // 第5层：领域压缩
            if context.estimated_tokens() > self.compression_threshold {
                let template = &self.domain_pack.compression_template;
                context.compress_with_template(template).await?;
            }
        }

        Ok(LoopResult { context })
    }
}
```

### Domain Pack 的注册方式

```rust
// AppBuilder 支持 Domain Pack
let app = AppBuilder::new()
    .provider(Arc::new(ProviderFactory::create(ModelConfig::anthropic(api_key, "claude-sonnet-4-6"))?))
    .domain_pack(coding_pack("/project/dir"))       // ← 一行切换领域
    // 或
    .domain_pack(research_pack())
    // 或
    .domain_pack(data_analysis_pack(&db_config))
    // 或混合多个领域
    .domain_packs(vec![
        coding_pack("/project/dir"),
        research_pack(),           // 编码时也需要查文档
    ])
    .approval_gate(Arc::new(ChannelApprovalGateWithThreshold::new(16, RiskLevel::Medium)))
    .build()?;
```

### 与 Claude Code 等的架构对比

| 维度 | Claude Code | OpenCode | Codex CLI | OneAI (提议) |
|------|-------------|----------|-----------|-------------|
| **工作流嵌入方式** | 硬编码编码工具+上下文 | 硬编码编码工具+SystemContext | 硬编码编码工具+沙箱 | **Domain Pack 可插拔配置** |
| **工具集** | 13 固定编码工具 | 13 固定编码工具 | 8 固定编码工具 | **领域工具包动态注册** |
| **上下文注入** | gitStatus 每轮全量 | Context Epoch 增量 | 初始注入+自然累积 | **ContextSource 独立可刷新** |
| **权限** | 三级(Read/Edit/Bash) | per-tool once/always/reject | suggest/auto-edit/full-auto | **PermissionProfile 领域配置** |
| **范式** | 6 固定子代理类型 | Skill.md + Agent配置 | 无子代理 | **ParadigmStrategy 领域映射** |
| **压缩** | harness 自动+compact命令 | LLM总结+结构化模板 | 截断+总结 | **CompressionTemplate 领域优先级** |
| **领域切换** | ❌ 仅编码 | ❌ 仅编码 | ❌ 仅编码 | **✅ 一行切换领域配置包** |

---

## 第四部分：关键设计决策

### 决策1：Domain Pack 是配置还是代码？

**推荐：混合模式**。Domain Pack 的骨架是 Rust 代码（工具实现、上下文源实现），但行为配置是**声明式的**：

```json
// oneai-domain.json — 领域配置文件（类似 OpenCode 的 opencode.json）
{
  "domain": "coding",
  "tools": ["read_code", "edit_code", "grep_code", "glob_code", "shell", "run_tests"],
  "context_sources": ["git_status", "file_tree", "project_config"],
  "permissions": {
    "auto_approve": ["read_code", "grep_code", "glob_code"],
    "confirm": ["edit_code", "write_code", "shell"],
    "deny": ["shell(rm*)", "shell(del*)"]
  },
  "system_prompt": "You are a coding assistant...",
  "compression_template": "coding_summary.md"
}
```

这样用户可以**创建自定义领域配置**（比如"智能家居控制"），而不需要写 Rust 代码——只需配置工具、上下文源、权限规则的组合。

### 决策2：通用工具 vs 领域特化工具？

以 `Read` 工具为例：

| 方案 | 描述 | 优缺点 |
|------|------|--------|
| **方案A: 通用 Read + 领域参数** | `ReadTool { path, offset, limit, domain_hint }` | ✅ 一个工具覆盖所有领域，❌ 参数爆炸，模型不知道何时用 offset/limit |
| **方案B: 领域 ReadCode / ReadPdf / ReadSensor** | 每个领域有专用 Read | ✅ 工具描述精确引导模型，❌ 工具数量多 |
| **方案C: 基础 Read + 领域装饰器** | `ReadTool` 是基础，DomainPack 添加 `domain_description` 和 `extra_params` | ✅ 平衡复用和特化 |

**推荐方案C**：基础工具提供通用能力，Domain Pack 通过**装饰器**添加领域特化：

```rust
// 基础工具
pub struct ReadTool { path, offset, limit }

// Domain Pack 裆饰：编码领域的 Read
DomainPack.decorate("read", ToolDecorator {
    description_override: "Read source code files. Supports line offset/limit for large files...",
    extra_params: {"encoding": "utf-8"},  // 编码领域特有参数
    risk_override: RiskLevel::Low,         // 编码领域：读代码=低风险
});

// Domain Pack 裆饰：数据分析领域的 Read
DomainPack.decorate("read", ToolDecorator {
    description_override: "Read data files (CSV, JSON, Parquet). Auto-detects format...",
    extra_params: {"format": "auto"},      // 数据分析领域特有参数
    risk_override: RiskLevel::Low,
});
```

### 决策3：跨领域混合如何处理？

用户可能需要**混合领域**——比如"编码+研究"（写代码时查文档），或"数据分析+IoT"（分析传感器数据并控制设备）。

**设计**：AppBuilder 支持 `domain_packs()` 注册多个 Pack，工具集合并、权限取最严格、上下文源全部注入：

```rust
let app = AppBuilder::new()
    .domain_packs(vec![
        coding_pack("/project/dir"),    // 编码工具 + 编码上下文 + 编码权限
        research_pack(),                // 研究工具 + 研究上下文 + 研究权限
    ])
    .build()?;

// 工具集合并: read_code + edit_code + web_search + web_fetch + ...
// 权限取严: shell 需确认（编码Pack）+ web_fetch 需确认URL（研究Pack）
// 上下文合并: git_status + file_tree + date + topic_context
// 系统提示合并: "你是编码和研究助手，可以同时编写代码和搜索文档"
```

---

## 第五部分：与 OneAI 当前架构的整合路径

### 当前 OneAI 的 gap → Domain Pack 的对应

| 当前 gap | Domain Pack 如何解决 |
|---------|---------------------|
| 工具太少(4个) | CodingPack 提供 8-10 个编码专用工具 |
| Shell 无安全过滤 | PermissionProfile.deny_by_default 过滤危险命令 |
| MCP 无实际集成 | ResearchPack 中 WebSearchTool/WebFetchTool 作为 MCP 代理工具 |
| 平台适配只有审批门 | IoTPack 中 CameraTool/ScreenshotTool 通过 PlatformTool trait 跨平台 |
| Agent 范式松散 | ParadigmStrategy 定义领域→范式的映射规则 |
| Demo 未端到端 | AppBuilder.domain_pack() + provider() 一行配置完整环境 |

### 实施优先级

| Phase | 内容 | 时间 |
|-------|------|------|
| **Phase 8** | DomainPack trait 定义 + CodingPack 实现 + AgenticLoop 重构 | 1-2月 |
| **Phase 9** | ResearchPack 实现（web_search/web_fetch MCP）+ ContextSource trait | 2-3月 |
| **Phase 10** | PermissionProfile 实现（命令过滤+领域分级）+ CompressionTemplate | 1月 |
| **Phase 11** | ParadigmStrategy + 子代理系统 + 端到端 Demo | 2-3月 |
| **Phase 12** | DataAnalysisPack + IoTPack + 混合领域支持 | 3-4月 |

**核心原则**：编码 Agent 把编码工作流嵌入框架的方式是"5层隐性配置"。OneAI 不需要硬编码任何领域的工作流——只需把这5层做成**可声明、可插拔、可组合的 Domain Pack 配置**，就能应对编码、研究、数据分析、IoT 控制等任何领域的工作流程。