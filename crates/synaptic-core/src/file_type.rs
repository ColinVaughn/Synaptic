use serde::{Deserialize, Serialize};

/// Node file/content classification — the set of valid `file_type` values.
/// Note: this is the *node-schema* set (includes `Rationale` and `Concept`);
/// the *detection-time* set in `synaptic-detect` instead has `Video` and no
/// rationale/concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Code,
    Document,
    Paper,
    Image,
    Rationale,
    Concept,
}

impl FileType {
    /// Map an arbitrary (possibly LLM-emitted) type string to a canonical
    /// `FileType` via a synonym table, with an unknown→`concept` fallback.
    pub fn from_lenient(s: &str) -> FileType {
        match s {
            "code" | "tool" | "library" => FileType::Code,
            "document" | "markdown" | "text" => FileType::Document,
            "paper" => FileType::Paper,
            "image" => FileType::Image,
            "rationale" => FileType::Rationale,
            "concept" | "pattern" | "principle" | "constraint" | "tech" | "technology"
            | "data-source" | "data_source" | "gotcha" | "framework" => FileType::Concept,
            _ => FileType::Concept,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_lowercase() {
        assert_eq!(serde_json::to_string(&FileType::Code).unwrap(), "\"code\"");
        assert_eq!(
            serde_json::to_string(&FileType::Rationale).unwrap(),
            "\"rationale\""
        );
        assert_eq!(
            serde_json::to_string(&FileType::Concept).unwrap(),
            "\"concept\""
        );
    }

    #[test]
    fn deserializes_lowercase() {
        let ft: FileType = serde_json::from_str("\"document\"").unwrap();
        assert_eq!(ft, FileType::Document);
    }

    #[test]
    fn lenient_maps_known_synonyms() {
        // Known synonym mappings.
        assert_eq!(FileType::from_lenient("markdown"), FileType::Document);
        assert_eq!(FileType::from_lenient("text"), FileType::Document);
        assert_eq!(FileType::from_lenient("tool"), FileType::Code);
        assert_eq!(FileType::from_lenient("library"), FileType::Code);
        assert_eq!(FileType::from_lenient("framework"), FileType::Concept);
        assert_eq!(FileType::from_lenient("data-source"), FileType::Concept);
    }

    #[test]
    fn lenient_passes_through_canonical() {
        assert_eq!(FileType::from_lenient("code"), FileType::Code);
        assert_eq!(FileType::from_lenient("paper"), FileType::Paper);
        assert_eq!(FileType::from_lenient("image"), FileType::Image);
    }

    #[test]
    fn lenient_unknown_defaults_to_concept() {
        // Unknown strings fall back to `concept`.
        assert_eq!(FileType::from_lenient("wat"), FileType::Concept);
        assert_eq!(FileType::from_lenient(""), FileType::Concept);
    }
}
