//! # OneAI Parser
//!
//! 3-layer output parsing defense:
//! - Layer 1: Constrained decoding (BNF grammar)
//! - Layer 2: Fuzzy JSON repair (bracket closing, regex extraction)
//! - Layer 3: Fallback self-correction loop

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.


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
}