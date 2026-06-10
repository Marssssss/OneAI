//! # OneAI Parser
//!
//! 3-layer output parsing defense:
//! - Layer 1: Constrained decoding (BNF grammar)
//! - Layer 2: Fuzzy JSON repair (bracket closing, regex extraction)
//! - Layer 3: Fallback self-correction loop

pub mod constrained;
pub mod fuzzy;
pub mod fallback;
pub mod three_layer;

pub use constrained::*;
pub use fuzzy::*;
pub use fallback::*;
pub use three_layer::*;

#[cfg(test)]
mod tests {
    use crate::fuzzy::FuzzyJsonRepair;
    use crate::three_layer::ThreeLayerParser;
    use oneai_core::traits::OutputParser;
    use oneai_core::ParsingLayer;
    use std::collections::HashMap;

    #[test]
    fn test_fuzzy_repair_valid_json() {
        let repair = FuzzyJsonRepair::new();
        let valid = "{\"name\": \"test\", \"value\": 42}";
        let result = repair.repair_and_parse(valid).unwrap();
        assert_eq!(result.get("name").unwrap().as_str().unwrap(), "test");
        assert_eq!(result.get("value").unwrap().as_u64().unwrap(), 42);
    }

    #[test]
    fn test_fuzzy_repair_unclosed_braces() {
        let repair = FuzzyJsonRepair::new();
        let broken = "{\"name\": \"test\", \"items\": [1, 2, 3";
        let result = repair.repair_and_parse(broken);
        assert!(result.is_ok());
    }

    #[test]
    fn test_fuzzy_repair_unclosed_array() {
        let repair = FuzzyJsonRepair::new();
        let broken = "[1, 2, 3";
        let result = repair.repair_and_parse(broken);
        assert!(result.is_ok());
        let val = result.unwrap();
        assert!(val.is_array());
    }

    #[test]
    fn test_fuzzy_repair_embedded_json() {
        let repair = FuzzyJsonRepair::new();
        let embedded = "Here is some text before the JSON: {\"key\": \"value\"} and some text after.";
        let result = repair.repair_and_parse(embedded);
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val.get("key").unwrap().as_str().unwrap(), "value");
    }

    #[test]
    fn test_fuzzy_repair_tool_call_format() {
        let repair = FuzzyJsonRepair::new();
        let tool_call = "{\"tool_calls\": [{\"id\": \"call_1\", \"name\": \"shell\", \"arguments\": {\"cmd\": \"ls\"}}]}";
        let result = repair.repair_and_parse(tool_call).unwrap();
        let tool_calls = result.get("tool_calls").unwrap().as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].get("name").unwrap().as_str().unwrap(), "shell");
    }

    #[test]
    fn test_fuzzy_repair_trailing_content() {
        let repair = FuzzyJsonRepair::new();
        let with_trailing = "{\"key\": \"value\"} some trailing garbage text";
        let result = repair.repair_and_parse(with_trailing);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_three_layer_parser_valid_json() {
        let parser = ThreeLayerParser::new();
        let valid_json = "{\"result\": \"success\"}";
        let result = parser.parse(valid_json, None).await.unwrap();
        assert_eq!(result.parsing_layer, ParsingLayer::FuzzyRepair);
    }

    #[test]
    fn test_hybrid_scorer() {
        use oneai_memory::HybridScorer;
        let scorer = HybridScorer::new();
        let score = scorer.score(0.9, 0.5);
        assert!((score - (0.7 * 0.9 + 0.3 * 0.5)).abs() < 0.001);
    }

    #[test]
    fn test_hybrid_scorer_custom_weights() {
        use oneai_memory::HybridScorer;
        let scorer = HybridScorer::with_weights(0.5, 0.5);
        let score = scorer.score(0.8, 0.6);
        assert!((score - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_short_term_memory() {
        use oneai_memory::ShortTermMemory;
        use oneai_core::MemoryEntry;

        let mut stm = ShortTermMemory::new(3);
        assert!(stm.is_empty());

        stm.push(MemoryEntry {
            id: "1".to_string(),
            content: "First".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        stm.push(MemoryEntry {
            id: "2".to_string(),
            content: "Second".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        assert_eq!(stm.len(), 2);

        stm.push(MemoryEntry {
            id: "3".to_string(),
            content: "Third".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        assert_eq!(stm.len(), 3);

        stm.push(MemoryEntry {
            id: "4".to_string(),
            content: "Fourth".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        });
        assert_eq!(stm.len(), 3);
        assert_eq!(stm.entries().front().unwrap().content, "Second");
    }

    #[test]
    fn test_scope_state() {
        use oneai_agent::ScopeState;
        use oneai_core::GlobalState;

        let global = GlobalState::new();
        let scope = ScopeState::from_global(&global);
        assert!(scope.global_memory.is_empty());
        assert!(scope.local_sandbox.is_empty());
        assert!(scope.pending_reductions.is_empty());
    }

    #[test]
    fn test_skill_selector_keyword_matching() {
        use oneai_skill::SkillSelector;
        use oneai_core::SkillDescriptor;

        let selector = SkillSelector::new();
        let skills = vec![
            SkillDescriptor {
                name: "shell".to_string(),
                description: "Execute shell commands".to_string(),
                prompt_template: "You can use shell.".to_string(),
                trigger_keywords: vec!["shell".to_string(), "command".to_string()],
                embedding: None,
            },
            SkillDescriptor {
                name: "code_review".to_string(),
                description: "Review code".to_string(),
                prompt_template: "You can review code.".to_string(),
                trigger_keywords: vec!["review".to_string(), "code".to_string()],
                embedding: None,
            },
            SkillDescriptor {
                name: "calculator".to_string(),
                description: "Calculate numbers".to_string(),
                prompt_template: "You can calculate.".to_string(),
                trigger_keywords: vec!["calculate".to_string(), "math".to_string()],
                embedding: None,
            },
        ];

        let result = tokio_test::block_on(selector.select_skills("I need to run a shell command", &skills)).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "shell");
    }
}