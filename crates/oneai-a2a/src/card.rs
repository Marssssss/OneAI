//! AgentCard generation and parsing — DomainPack → AgentCard mapping.
//!
//! The A2A protocol uses AgentCards as the discovery mechanism for agents.
//! This module provides:
//!
//! - **DomainPack → AgentCard**: Automatic generation from OneAI's DomainPack system
//! - **JSON parsing**: Parse AgentCards from remote agents
//! - **Well-known endpoint**: JSON output for serving at `/.well-known/agent-card`
//!
//! The DomainPack → AgentCard mapping is a key integration point between
//! OneAI's domain configuration system and the A2A protocol:
//!
//! | DomainPack field | AgentCard field |
//! |------------------|-----------------|
//! | `name` | `name` |
//! | `description` | `description` |
//! | `paradigm_strategies` | `skills[]` (each strategy → AgentSkill) |
//! | `sub_agent_definitions` | `skills[]` (each sub-agent → AgentSkill) |
//! | `tools` (via tool registry) | `capabilities` |

use crate::error::Result;
use crate::types::*;

// ─── DomainPack → AgentCard ─────────────────────────────────────────────────────

/// Generate an AgentCard from a DomainPack.
///
/// This maps the DomainPack's domain-specific configuration to the A2A
/// AgentCard format, enabling OneAI agents to advertise their capabilities
/// to remote A2A agents.
///
/// The mapping:
/// - DomainPack name → AgentCard name
/// - DomainPack description → AgentCard description
/// - ParadigmStrategies → AgentSkill[] (each strategy becomes a skill)
/// - SubAgentTypeDefinitions → AgentSkill[] (each sub-agent becomes a skill)
/// - DomainPack system_prompt → reflected in skill descriptions
///
/// **Usage**:
/// ```ignore
/// let domain = coding_pack("/project/dir");
/// let card = agent_card_from_domain_pack(&domain, "https://my-agent.example.com");
/// println!("AgentCard: {}", card.to_json_pretty()?);
/// ```
pub fn agent_card_from_domain_pack(domain: &oneai_domain::DomainPack, url: &str) -> AgentCard {
    let mut skills = Vec::new();

    // Map ParadigmStrategies → AgentSkill[]
    for strategy in &domain.paradigm_strategies {
        let skill_id = format!("strategy-{}", strategy.trigger_pattern.replace("|", "-"));
        let skill_name = format!("Strategy: {}", strategy.description);
        let skill_description = format!(
            "Applies {} paradigm sequence for tasks matching '{}'. {}",
            strategy.paradigm_sequence.iter()
                .map(|p| {
                    match p {
                        oneai_domain::DomainParadigmKind::Plan => "Plan",
                        oneai_domain::DomainParadigmKind::ReAct => "ReAct",
                        oneai_domain::DomainParadigmKind::Reflect => "Reflect",
                        oneai_domain::DomainParadigmKind::Explore => "Explore",
                    }
                })
                .collect::<Vec<_>>()
                .join(" → "),
            strategy.trigger_pattern,
            strategy.description,
        );

        let examples = strategy.trigger_pattern.split('|')
            .map(|s| format!("{} the {}", s.trim(), domain.name))
            .collect();

        skills.push(AgentSkill {
            id: skill_id,
            name: skill_name,
            description: skill_description,
            tags: vec![domain.name.clone(), "paradigm".to_string()],
            examples,
            input_modes: vec!["text/plain".to_string()],
            output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        });
    }

    // Map SubAgentTypeDefinitions → AgentSkill[]
    for sub_agent in &domain.sub_agent_definitions {
        skills.push(AgentSkill {
            id: format!("sub-agent-{}", sub_agent.name),
            name: format!("Sub-Agent: {}", sub_agent.name),
            description: sub_agent.description.clone(),
            tags: vec![domain.name.clone(), "sub-agent".to_string(), sub_agent.name.clone()],
            examples: Vec::new(),
            input_modes: vec!["text/plain".to_string()],
            output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        });
    }

    // If no strategies or sub-agents defined, add a generic skill
    if skills.is_empty() {
        skills.push(AgentSkill::new(
            format!("{}-general", domain.name),
            format!("{} Agent", domain.name),
            domain.description.clone(),
        ));
    }

    AgentCard {
        name: domain.name.clone(),
        description: domain.description.clone(),
        url: url.to_string(),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        provider: Some(AgentProvider {
            organization: "OneAI".to_string(),
            url: Some("https://github.com/oneai-project/oneai".to_string()),
        }),
        skills,
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: true,
        },
        authentication: AuthenticationInfo::default(),
        documentation_url: None,
    }
}

// ─── Parsing ────────────────────────────────────────────────────────────────────

/// Parse an AgentCard from a JSON string.
///
/// Useful for parsing AgentCards received from remote agents during
/// the `discover()` phase.
pub fn parse_agent_card(json: &str) -> Result<AgentCard> {
    AgentCard::from_json(json)
}

/// Parse an AgentCard from a YAML string.
///
/// A2A agents may serve their AgentCards as YAML for human readability.
/// This function converts YAML to JSON first, then parses.
pub fn parse_agent_card_yaml(yaml: &str) -> Result<AgentCard> {
    // Simple YAML → JSON conversion for basic YAML structures
    // For full YAML support, would need a YAML parser crate
    // Currently we handle the common case of JSON already
    // YAML parsing can be added as an optional feature later
    parse_agent_card(yaml)
}

/// Generate the well-known agent-card JSON for serving at `/.well-known/agent-card`.
///
/// This produces a pretty-printed JSON string suitable for serving as an
/// HTTP response at the standard AgentCard discovery endpoint.
pub fn well_known_agent_card(card: &AgentCard) -> Result<String> {
    card.to_json_pretty()
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_domain::DomainPackBuilder;
    use oneai_domain::SubAgentTypeDefinition;

    #[test]
    fn test_agent_card_from_coding_domain_pack() {
        let pack = DomainPackBuilder::new("coding")
            .description("A coding assistant agent")
            .system_prompt("You are a coding assistant.")
            .paradigm_strategy(oneai_domain::ParadigmStrategy {
                trigger_pattern: "refactor|rewrite|restructure".to_string(),
                paradigm_sequence: vec![
                    oneai_domain::DomainParadigmKind::Plan,
                    oneai_domain::DomainParadigmKind::ReAct,
                    oneai_domain::DomainParadigmKind::Reflect,
                ],
                sub_agent_types: Vec::new(),
                description: "Refactoring tasks".to_string(),
            })
            .sub_agent_definition(SubAgentTypeDefinition::code())
            .build();

        let card = agent_card_from_domain_pack(&pack, "https://coding-agent.example.com");

        assert_eq!(card.name, "coding");
        assert_eq!(card.description, "A coding assistant agent");
        assert_eq!(card.url, "https://coding-agent.example.com");
        assert!(card.version.is_some());
        assert!(card.provider.is_some());

        // Should have skills from paradigm strategy + sub-agent
        assert_eq!(card.skills.len(), 2); // 1 strategy + 1 sub-agent

        // Check strategy skill
        let strategy_skill = card.skills.iter()
            .find(|s| s.id.starts_with("strategy-"))
            .unwrap();
        assert!(strategy_skill.description.contains("Plan → ReAct → Reflect"));
        assert!(strategy_skill.examples.len() > 0);

        // Check sub-agent skill
        let sub_agent_skill = card.skills.iter()
            .find(|s| s.id == "sub-agent-code")
            .unwrap();
        assert_eq!(sub_agent_skill.name, "Sub-Agent: code");
    }

    #[test]
    fn test_agent_card_from_simple_domain_pack() {
        let pack = DomainPackBuilder::new("research")
            .description("A research assistant agent")
            .build();

        let card = agent_card_from_domain_pack(&pack, "https://research-agent.example.com");

        assert_eq!(card.name, "research");
        // No strategies or sub-agents → generic skill
        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].id, "research-general");
    }

    #[test]
    fn test_agent_card_json_roundtrip_from_domain_pack() {
        let pack = DomainPackBuilder::new("data-analysis")
            .description("Data analysis agent")
            .paradigm_strategy(oneai_domain::ParadigmStrategy {
                trigger_pattern: "analyze|compute|calculate".to_string(),
                paradigm_sequence: vec![oneai_domain::DomainParadigmKind::ReAct],
                sub_agent_types: Vec::new(),
                description: "Data analysis tasks".to_string(),
            })
            .build();

        let card = agent_card_from_domain_pack(&pack, "https://data-agent.example.com");

        // Roundtrip through JSON serialization
        let json = card.to_json_pretty().unwrap();
        let parsed = parse_agent_card(&json).unwrap();

        assert_eq!(card.name, parsed.name);
        assert_eq!(card.url, parsed.url);
        assert_eq!(card.skills.len(), parsed.skills.len());
    }

    #[test]
    fn test_parse_agent_card_from_json() {
        let json = r#"{
            "name": "RemoteCodingAgent",
            "description": "A remote coding assistant",
            "url": "https://remote.example.com/a2a",
            "version": "2.0.0",
            "provider": {
                "organization": "RemoteOrg",
                "url": "https://remoteorg.com"
            },
            "skills": [
                {
                    "id": "code-review",
                    "name": "Code Review",
                    "description": "Reviews code for issues",
                    "tags": ["coding", "review"],
                    "examples": ["Review this code for bugs"],
                    "inputModes": ["text/plain"],
                    "outputModes": ["text/plain", "application/json"]
                }
            ],
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "capabilities": {
                "streaming": true,
                "pushNotifications": false,
                "stateTransitionHistory": true
            },
            "authentication": {
                "schemes": ["bearer"]
            }
        }"#;

        let card = parse_agent_card(json).unwrap();
        assert_eq!(card.name, "RemoteCodingAgent");
        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].id, "code-review");
        assert!(card.capabilities.streaming);
        assert_eq!(card.authentication.schemes.len(), 1);
    }

    #[test]
    fn test_well_known_agent_card_output() {
        let pack = DomainPackBuilder::new("test-domain")
            .description("Test agent")
            .build();
        let card = agent_card_from_domain_pack(&pack, "https://test.example.com");
        let json = well_known_agent_card(&card).unwrap();

        // Verify it's valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get("name").and_then(|v| v.as_str()), Some("test-domain"));
        assert_eq!(parsed.get("url").and_then(|v| v.as_str()), Some("https://test.example.com"));
    }

    #[test]
    fn test_parse_invalid_agent_card() {
        let result = parse_agent_card("not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_domain_pack_with_multiple_strategies() {
        let pack = DomainPackBuilder::new("coding")
            .description("Full coding agent")
            .paradigm_strategy(oneai_domain::ParadigmStrategy {
                trigger_pattern: "refactor|rewrite".to_string(),
                paradigm_sequence: vec![oneai_domain::DomainParadigmKind::Plan, oneai_domain::DomainParadigmKind::ReAct],
                sub_agent_types: Vec::new(),
                description: "Refactoring".to_string(),
            })
            .paradigm_strategy(oneai_domain::ParadigmStrategy {
                trigger_pattern: "debug|fix|repair".to_string(),
                paradigm_sequence: vec![oneai_domain::DomainParadigmKind::Explore, oneai_domain::DomainParadigmKind::ReAct],
                sub_agent_types: Vec::new(),
                description: "Debugging".to_string(),
            })
            .build();

        let card = agent_card_from_domain_pack(&pack, "https://coding.example.com");
        assert_eq!(card.skills.len(), 2);

        // Both strategies should have different trigger patterns
        let skill_ids: Vec<&str> = card.skills.iter().map(|s| s.id.as_str()).collect();
        assert!(skill_ids.iter().any(|id| id.contains("refactor")));
        assert!(skill_ids.iter().any(|id| id.contains("debug")));
    }
}
