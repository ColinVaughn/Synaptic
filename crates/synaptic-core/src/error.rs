use thiserror::Error;

/// Errors produced by the core contract layer.
#[derive(Debug, Error)]
pub enum CoreError {
    /// JSON (de)serialization failure.
    #[error("JSON (de)serialization failed: {0}")]
    Json(#[from] serde_json::Error),

    /// Extraction JSON failed schema validation; carries one message per violation.
    #[error("extraction failed schema validation with {} error(s)", errors.len())]
    Validation { errors: Vec<String> },
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_error_reports_count() {
        let e = CoreError::Validation {
            errors: vec!["a".to_string(), "b".to_string()],
        };
        assert_eq!(
            e.to_string(),
            "extraction failed schema validation with 2 error(s)"
        );
    }

    #[test]
    fn json_error_converts_via_from() {
        let parse: std::result::Result<serde_json::Value, _> = serde_json::from_str("{not json");
        let core: CoreError = parse.unwrap_err().into();
        assert!(matches!(core, CoreError::Json(_)));
    }
}
