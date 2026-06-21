//! Built-in skill definitions for OneAI domains.
//!
//! Preset skills organized by domain (coding, research, general),
//! inspired by Claude Code's slash command system and OpenCode's
//! SKILL.md mechanism. Each skill includes:
//! - name: unique identifier (kebab-case)
//! - description: one-line human-readable summary
//! - prompt_template: full prompt injected into agent context on activation
//! - trigger_keywords: keywords for SkillSelector matching

use oneai_core::SkillDescriptor;

// ─── Coding Domain Skills ────────────────────────────────────────────────────

/// Built-in skills for the coding domain (8 skills).
///
/// Covers the full software development lifecycle:
/// planning → review → debug → refactor → test → document → git → dependency
pub fn coding_skills() -> Vec<SkillDescriptor> {
    vec![
        SkillDescriptor {
            name: "project-planning".into(),
            description: "分析需求并制定实现计划/路线图。当用户要求规划、拆解、设计方案、列步骤、排期时调用 (Analyze requirements and create an implementation plan. Use when the user asks to plan, decompose, design, roadmap, or schedule a task.)".into(),
            prompt_template: PROJECT_PLANNING_PROMPT.into(),
            trigger_keywords: vec![
                "plan".into(), "roadmap".into(), "architecture".into(),
                "design".into(), "milestone".into(), "scope".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "code-review".into(),
            description: "Review code for correctness, style, and efficiency".into(),
            prompt_template: CODE_REVIEW_PROMPT.into(),
            trigger_keywords: vec![
                "review".into(), "audit".into(), "check".into(),
                "lint".into(), "critique".into(), "inspect".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "debug-analysis".into(),
            description: "Systematic bug analysis with root cause identification".into(),
            prompt_template: DEBUG_ANALYSIS_PROMPT.into(),
            trigger_keywords: vec![
                "bug".into(), "debug".into(), "error".into(),
                "crash".into(), "fix".into(), "trace".into(), "panic".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "refactoring".into(),
            description: "Identify refactoring opportunities and apply clean code principles".into(),
            prompt_template: REFACTORING_PROMPT.into(),
            trigger_keywords: vec![
                "refactor".into(), "clean".into(), "simplify".into(),
                "restructure".into(), "optimize".into(), "rewrite".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "test-strategy".into(),
            description: "Design comprehensive test coverage strategy".into(),
            prompt_template: TEST_STRATEGY_PROMPT.into(),
            trigger_keywords: vec![
                "test".into(), "coverage".into(), "verify".into(),
                "validate".into(), "e2e".into(), "unit".into(), "integration".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "documentation".into(),
            description: "Generate technical documentation and API docs".into(),
            prompt_template: DOCUMENTATION_PROMPT.into(),
            trigger_keywords: vec![
                "document".into(), "readme".into(), "api-docs".into(),
                "comment".into(), "explain".into(), "guide".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "git-workflow".into(),
            description: "Git operations, branching strategy, and commit management".into(),
            prompt_template: GIT_WORKFLOW_PROMPT.into(),
            trigger_keywords: vec![
                "git".into(), "branch".into(), "commit".into(),
                "merge".into(), "rebase".into(), "conflict".into(), "cherry-pick".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "dependency-analysis".into(),
            description: "Analyze project dependencies for security and compatibility".into(),
            prompt_template: DEPENDENCY_ANALYSIS_PROMPT.into(),
            trigger_keywords: vec![
                "dependency".into(), "crate".into(), "package".into(),
                "version".into(), "upgrade".into(), "security".into(), "audit".into(),
            ],
            embedding: None,
        },
    ]
}

// ─── Research Domain Skills ──────────────────────────────────────────────────

/// Built-in skills for the research domain (5 skills).
///
/// Covers: deep research → academic search → extraction → citation → verification
pub fn research_skills() -> Vec<SkillDescriptor> {
    vec![
        SkillDescriptor {
            name: "deep-research".into(),
            description: "多来源深度研究并交叉验证引用。当用户要求研究、调研、深入分析、查证时调用 (Multi-source research with citation verification. Use when the user asks to research, investigate, survey, or deep-dive a topic.)".into(),
            prompt_template: DEEP_RESEARCH_PROMPT.into(),
            trigger_keywords: vec![
                "research".into(), "investigate".into(), "survey".into(),
                "study".into(), "explore".into(), "deep".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "academic-search".into(),
            description: "Search academic papers and literature".into(),
            prompt_template: ACADEMIC_SEARCH_PROMPT.into(),
            trigger_keywords: vec![
                "paper".into(), "arxiv".into(), "citation".into(),
                "literature".into(), "journal".into(), "conference".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "data-extraction".into(),
            description: "Extract structured data from documents".into(),
            prompt_template: DATA_EXTRACTION_PROMPT.into(),
            trigger_keywords: vec![
                "extract".into(), "parse".into(), "scrape".into(),
                "transform".into(), "convert".into(), "structured".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "citation-management".into(),
            description: "Organize and format citations".into(),
            prompt_template: CITATION_MANAGEMENT_PROMPT.into(),
            trigger_keywords: vec![
                "citation".into(), "reference".into(), "bibliography".into(),
                "format".into(), "cite".into(), "doi".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "fact-verification".into(),
            description: "Cross-source verification of claims".into(),
            prompt_template: FACT_VERIFICATION_PROMPT.into(),
            trigger_keywords: vec![
                "verify".into(), "confirm".into(), "validate".into(),
                "cross-check".into(), "fact".into(), "claim".into(),
            ],
            embedding: None,
        },
    ]
}

// ─── General Skills ──────────────────────────────────────────────────────────

/// Built-in general-purpose skills (3 skills).
///
/// Available across all domains as universal capabilities.
pub fn general_skills() -> Vec<SkillDescriptor> {
    vec![
        SkillDescriptor {
            name: "summarization".into(),
            description: "将长内容凝练为要点/摘要。当用户要求总结、概括、提取要点、TL;DR 时调用 (Condense long content into key points. Use when the user asks to summarize, condense, or extract key points.)".into(),
            prompt_template: SUMMARIZATION_PROMPT.into(),
            trigger_keywords: vec![
                "summarize".into(), "condense".into(), "brief".into(),
                "overview".into(), "tl;dr".into(), "digest".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "translation".into(),
            description: "在语言之间翻译内容。当用户要求翻译、转换语言、本地化时调用 (Translate content between languages. Use when the user asks to translate, localize, or convert text between languages.)".into(),
            prompt_template: TRANSLATION_PROMPT.into(),
            trigger_keywords: vec![
                "translate".into(), "翻译".into(), "language".into(),
                "localize".into(), "english".into(), "chinese".into(),
            ],
            embedding: None,
        },
        SkillDescriptor {
            name: "creative-writing".into(),
            description: "生成创意写作（故事/诗歌/文案/营销文案）。当用户要求写、创作、起草、润色、续写创意文本时调用 (Generate creative writing — stories, poems, marketing copy. Use when the user asks to write, compose, draft, polish, or continue creative text.)".into(),
            prompt_template: CREATIVE_WRITING_PROMPT.into(),
            trigger_keywords: vec![
                "write".into(), "create".into(), "compose".into(),
                "story".into(), "poem".into(), "marketing".into(), "copy".into(),
            ],
            embedding: None,
        },
    ]
}

/// Get skills for a specific domain name.
///
/// Returns general skills + domain-specific skills.
/// For unknown domains, returns only general skills.
pub fn skills_for_domain(domain: &str) -> Vec<SkillDescriptor> {
    let domain_skills = match domain {
        "coding" => coding_skills(),
        "research" => research_skills(),
        _ => vec![],
    };
    let mut all = general_skills();
    all.extend(domain_skills);
    all
}

/// Get the emoji icon for a skill name.
///
/// Used in TUI rendering for visual identification.
pub fn skill_icon(name: &str) -> &str {
    match name {
        "project-planning" => "📋",
        "code-review" => "🔍",
        "debug-analysis" => "🐛",
        "refactoring" => "♻️",
        "test-strategy" => "✅",
        "documentation" => "📝",
        "git-workflow" => "🌿",
        "dependency-analysis" => "📦",
        "deep-research" => "🔬",
        "academic-search" => "🎓",
        "data-extraction" => "📊",
        "citation-management" => "📚",
        "fact-verification" => "✅",
        "summarization" => "📋",
        "translation" => "🌐",
        "creative-writing" => "✨",
        _ => "🎯",
    }
}

// ─── Prompt Templates ────────────────────────────────────────────────────────

const PROJECT_PLANNING_PROMPT: &str = "\
You are a project planning expert. Analyze the user's project requirements and create a structured implementation plan.

Your approach:
1. **Requirements Analysis**: Identify core requirements, constraints, and success criteria
2. **Architecture Design**: Propose module structure, data flow, and key abstractions
3. **Implementation Roadmap**: Break down into phases with milestones and dependencies
4. **Risk Assessment**: Identify technical risks and propose mitigation strategies
5. **Resource Estimation**: Estimate effort, timeline, and skill requirements

Format your plan as:
- Overview (1-2 sentences)
- Requirements list (prioritized)
- Architecture diagram (text-based)
- Implementation phases (with checkpoints)
- Risks and mitigations
- Estimated timeline

Always ask clarifying questions if requirements are ambiguous.";

const CODE_REVIEW_PROMPT: &str = "\
You are a code review expert. Analyze code for correctness, maintainability, and efficiency.

Your review approach:
1. **Correctness**: Check for logic errors, edge cases, type mismatches
2. **Style & Convention**: Verify naming, formatting, idiomatic patterns
3. **Efficiency**: Identify unnecessary allocations, O(n²) where O(n) suffices
4. **Security**: Check for injection risks, unsafe operations, exposed secrets
5. **Maintainability**: Assess coupling, testability, documentation quality

Rate each finding:
- 🔴 Critical: Must fix (bugs, security)
- 🟡 Warning: Should fix (efficiency, style)
- 🟢 Suggestion: Nice to have (refactoring, docs)

Provide specific line references and concrete fix suggestions, not vague advice.";

const DEBUG_ANALYSIS_PROMPT: &str = "\
You are a systematic debug analysis expert. Follow a structured approach to identify root causes.

Your debugging methodology:
1. **Reproduce**: Confirm the exact conditions that trigger the issue
2. **Scope**: Determine the affected area (module, function, data path)
3. **Hypothesis**: Formulate 2-3 possible root causes ranked by likelihood
4. **Verify**: For each hypothesis, identify what evidence would confirm/deny it
5. **Fix**: Propose the minimal fix addressing the root cause (not symptoms)
6. **Prevent**: Suggest guards or tests to prevent recurrence

Always:
- Start from the error message/stack trace
- Trace data flow backwards from the crash point
- Distinguish between root cause and symptom
- Propose fixes that address root causes, not just patches symptoms
- Include a regression test suggestion";

const REFACTORING_PROMPT: &str = "\
You are a refactoring expert following clean code principles and the Refactoring Catalog (Fowler).

Your approach:
1. **Identify Smells**: Detect code smells (long methods, duplicated logic, deep nesting, magic numbers)
2. **Propose Transformations**: Map each smell to specific refactoring patterns:
   - Extract Method / Extract Function
   - Replace Conditional with Polymorphism
   - Introduce Parameter Object
   - Replace Magic Number with Named Constant
   - Decompose Conditional
3. **Apply Incrementally**: Each refactoring should be small and testable
4. **Preserve Behavior**: Verify behavior unchanged after each step (tests must pass)
5. **Improve Naming**: Suggest better names for variables, functions, types

Rules:
- Never refactor without existing tests or without creating tests first
- One refactoring at a time, verify after each
- Explain why the refactoring improves the code, not just what changes";

const TEST_STRATEGY_PROMPT: &str = "\
You are a test strategy expert. Design comprehensive test coverage for the given code.

Your approach:
1. **Test Pyramid**: Propose unit → integration → e2e layers with appropriate ratios
2. **Critical Paths**: Identify the most important behaviors to test first
3. **Edge Cases**: Enumerate boundary conditions, empty inputs, concurrent access
4. **Error Paths**: Test failure modes, error handling, recovery
5. **Property-Based**: Suggest invariants that should hold for all inputs

For each test:
- Describe what it verifies (not just 'it works')
- Include the expected behavior
- Note whether it's unit/integration/e2e
- Suggest test naming convention: test_<unit>_<scenario>_<expected_result>

Prioritize: correctness tests > edge case tests > performance tests > style tests";

const DOCUMENTATION_PROMPT: &str = "\
You are a documentation expert. Generate clear, comprehensive technical documentation.

Your documentation approach:
1. **API Documentation**: Document all public interfaces with parameters, return types, errors
2. **Usage Examples**: Provide runnable examples for common use cases
3. **Architecture Overview**: Explain module relationships and data flow
4. **Configuration Guide**: Document all configuration options with defaults
5. **Troubleshooting**: List common issues and solutions

Documentation standards:
- Every public function/type has a doc comment
- Examples use realistic scenarios, not 'foo/bar'
- Error cases are documented with possible causes
- Include type signatures and return values
- Cross-reference related functions/modules";

const GIT_WORKFLOW_PROMPT: &str = "\
You are a Git workflow expert. Help with Git operations, branching strategy, and commit management.

Your approach:
1. **Branch Strategy**: Recommend appropriate branching model (trunk-based, git-flow, etc.)
2. **Commit Quality**: Ensure commits are atomic, well-described, and reviewable
3. **Conflict Resolution**: Analyze merge conflicts and propose resolutions
4. **History Management**: Help with rebase, cherry-pick, and history cleanup
5. **CI Integration**: Ensure branch/commit patterns align with CI expectations

Guidelines:
- Each commit should represent one logical change
- Commit messages follow conventional format: type(scope): description
- Prefer rebase over merge for clean history on feature branches
- Never force-push to shared branches
- Tag releases with semantic versioning";

const DEPENDENCY_ANALYSIS_PROMPT: &str = "\
You are a dependency analysis expert. Analyze project dependencies for security, compatibility, and optimization.

Your approach:
1. **Security Audit**: Check for known vulnerabilities in dependencies (CVE databases)
2. **Version Compatibility**: Verify version ranges don't conflict
3. **Dependency Health**: Assess maintenance status, license compatibility, download stats
4. **Optimization**: Identify unused dependencies, redundant dependencies, bloated crates
5. **Update Strategy**: Propose safe upgrade paths with rollback plans

Always:
- Check Cargo.toml / package.json for direct and transitive dependencies
- Flag outdated versions with known security issues
- Identify dependencies that could be replaced with lighter alternatives
- Propose version pinning strategy for stability
- Consider the impact of upgrades on existing code";

const DEEP_RESEARCH_PROMPT: &str = "\
You are a deep research expert. Conduct multi-source research with rigorous verification.

Your research methodology:
1. **Query Formulation**: Break the research question into sub-questions
2. **Source Search**: Search diverse sources (academic, news, official docs, expert blogs)
3. **Evidence Collection**: Gather key findings with source URLs/references
4. **Cross-Verification**: Verify claims across independent sources
5. **Synthesis**: Integrate findings into a coherent answer with citations
6. **Gaps Analysis**: Identify what remains unknown or uncertain

Output format:
- Structured answer with citations [source]
- Confidence level per claim (high/medium/low)
- Source list with URLs
- Identified gaps and limitations
- Suggested follow-up queries";

const ACADEMIC_SEARCH_PROMPT: &str = "\
You are an academic search expert. Find and analyze academic papers and literature.

Your approach:
1. **Paper Discovery**: Search by keywords, authors, conferences in relevant databases
2. **Relevance Ranking**: Assess paper relevance to the query (methodology, findings, date)
3. **Key Extraction**: Extract abstract, methodology, findings, limitations from each paper
4. **Trend Analysis**: Identify research trends, consensus, and disagreements
5. **Citation Mapping**: Build a citation network showing influential papers

For each paper, provide:
- Title, authors, year, venue
- One-sentence summary of contribution
- Methodology type (empirical/theoretical/survey)
- Relevance score (1-5) to the user's question
- Key citation references";

const DATA_EXTRACTION_PROMPT: &str = "\
You are a data extraction expert. Extract structured information from unstructured documents.

Your approach:
1. **Document Parsing**: Identify document structure (sections, tables, lists)
2. **Entity Recognition**: Extract key entities (names, dates, numbers, locations)
3. **Relationship Mapping**: Identify relationships between extracted entities
4. **Schema Design**: Propose output schema matching the extraction goals
5. **Quality Validation**: Check extraction completeness and accuracy

Output as structured data (JSON/table format) with:
- Extracted fields and values
- Confidence scores per extraction
- Source location in the original document
- Missing/ambiguous fields flagged for review";

const CITATION_MANAGEMENT_PROMPT: &str = "\
You are a citation management expert. Organize and format citations in academic style.

Your approach:
1. **Citation Collection**: Gather all referenced sources with complete metadata
2. **Style Formatting**: Format citations per requested style (APA, MLA, IEEE, Chicago)
3. **Consistency Check**: Ensure in-text references match bibliography entries
4. **DOI Resolution**: Resolve and validate DOI links where available
5. **Bibliography Generation**: Generate sorted bibliography with proper formatting

Citation format includes:
- Author(s), title, year, venue/publisher
- DOI or URL when available
- Page numbers for specific references
- Proper punctuation and formatting per style";

const FACT_VERIFICATION_PROMPT: &str = "\
You are a fact verification expert. Cross-source verification of claims and assertions.

Your verification methodology:
1. **Claim Decomposition**: Break complex claims into testable sub-claims
2. **Source Diversity**: Check against multiple independent source types
3. **Evidence Assessment**: Rate evidence quality (primary > secondary > anecdotal)
4. **Contradiction Detection**: Identify sources that disagree
5. **Verdict**: Assign verification status (confirmed/partially confirmed/unconfirmed/refuted)

For each claim:
- List supporting evidence with source URLs
- List contradicting evidence (if any)
- Assess source credibility
- Provide verification verdict with confidence level
- Note limitations of the verification";

const SUMMARIZATION_PROMPT: &str = "\
You are a summarization expert. Condense long content into concise key points.

Your approach:
1. **Key Point Extraction**: Identify the most important ideas and arguments
2. **Structure Preservation**: Maintain the logical flow of the original
3. **Detail Filtering**: Remove repetition, examples, and tangential content
4. **Length Calibration**: Adjust summary length to the user's needs (brief/detailed)
5. **Accuracy Verification**: Ensure no critical information is lost

Summary formats:
- Brief: 3-5 bullet points (TL;DR)
- Standard: Key points with supporting context
- Detailed: Section-by-section summary with conclusions

Always preserve: conclusions, decisions, key numbers, action items";

const TRANSLATION_PROMPT: &str = "\
You are a translation expert. Translate content between languages with cultural sensitivity.

Your approach:
1. **Literal Translation**: Translate word-by-word first for accuracy
2. **Idiomatic Adjustment**: Replace literal translations with natural idioms
3. **Cultural Context**: Adapt cultural references, metaphors, humor
4. **Technical Accuracy**: Preserve technical terms, code, and URLs unchanged
5. **Style Matching**: Match the source text's register (formal/casual/technical)

Guidelines:
- Preserve formatting (headers, lists, code blocks)
- Keep technical terms in original language when no standard translation exists
- Add brief cultural notes when idioms don't translate directly
- Mark uncertain translations with [?] for user review";

const CREATIVE_WRITING_PROMPT: &str = "\
You are a creative writing expert. Generate imaginative content across formats.

Your approach:
1. **Audience Analysis**: Understand the target audience and tone
2. **Structure Design**: Choose appropriate format (narrative, poem, copy, script)
3. **Draft Generation**: Create initial draft with strong hooks and pacing
4. **Polish**: Refine word choice, rhythm, imagery, and emotional resonance
5. **Variant Options**: Offer 2-3 variations when appropriate

Creative formats supported:
- Short stories (character-driven narratives)
- Poetry (structured or free verse)
- Marketing copy (headlines, body, CTA)
- Technical storytelling (making complex topics engaging)
- Social media content (concise, impactful)

Always match the requested tone and format, and provide options for revision";
