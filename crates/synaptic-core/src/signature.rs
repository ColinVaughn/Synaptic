use serde::{Deserialize, Serialize};

/// One parameter in a function signature. `type_ref` holds the annotation text
/// when the source provides one: present for typed languages (Rust, Java, Go,
/// annotated Python/TS) and absent for unannotated parameters in dynamically
/// typed code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub type_ref: Option<String>,
}

/// A function or method signature captured during extraction.
///
/// `raw` is the verbatim signature text and is always populated, so a
/// description is never empty even when the grammar does not let us split out
/// individual parameters or a return type. `params` and `return_type` are the
/// structured breakdown when available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub params: Vec<Param>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub return_type: Option<String>,
    pub raw: String,
}

impl Signature {
    /// The number of declared parameters.
    pub fn arity(&self) -> usize {
        self.params.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_and_omits_empty_type_ref() {
        let sig = Signature {
            params: vec![
                Param {
                    name: "x".into(),
                    type_ref: Some("u32".into()),
                },
                Param {
                    name: "y".into(),
                    type_ref: None,
                },
            ],
            return_type: None,
            raw: "(x: u32, y)".into(),
        };
        assert_eq!(sig.arity(), 2);
        let v = serde_json::to_value(&sig).unwrap();
        // Untyped param omits type_ref; absent return_type omitted too.
        assert!(!v["params"][1].as_object().unwrap().contains_key("type_ref"));
        assert!(!v.as_object().unwrap().contains_key("return_type"));
        let back: Signature = serde_json::from_value(v).unwrap();
        assert_eq!(back, sig);
    }
}
