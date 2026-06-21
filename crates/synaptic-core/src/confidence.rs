use serde::{Deserialize, Serialize};

/// Relationship confidence. Serializes as the canonical uppercase strings
/// (`EXTRACTED`, `INFERRED`, `AMBIGUOUS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Confidence {
    Extracted,
    Inferred,
    Ambiguous,
}

impl Confidence {
    /// Numeric fallback score for this level.
    pub fn default_score(self) -> f32 {
        match self {
            Confidence::Extracted => 1.0,
            Confidence::Inferred => 0.5,
            Confidence::Ambiguous => 0.2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_uppercase() {
        assert_eq!(
            serde_json::to_string(&Confidence::Extracted).unwrap(),
            "\"EXTRACTED\""
        );
        assert_eq!(
            serde_json::to_string(&Confidence::Inferred).unwrap(),
            "\"INFERRED\""
        );
        assert_eq!(
            serde_json::to_string(&Confidence::Ambiguous).unwrap(),
            "\"AMBIGUOUS\""
        );
    }

    #[test]
    fn deserializes_uppercase() {
        let c: Confidence = serde_json::from_str("\"INFERRED\"").unwrap();
        assert_eq!(c, Confidence::Inferred);
    }

    #[test]
    fn default_scores_are_correct() {
        // Per-level fallback scores.
        assert_eq!(Confidence::Extracted.default_score(), 1.0);
        assert_eq!(Confidence::Inferred.default_score(), 0.5);
        assert_eq!(Confidence::Ambiguous.default_score(), 0.2);
    }
}
