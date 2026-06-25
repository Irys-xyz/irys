//! RFC 9457 Problem Details — the canonical HTTP error shape for the Irys API.
//!
//! Defined here in `irys-types` so the API server and every client share one
//! type. The server populates `trace_id` from its OTEL span; the type itself
//! stays dependency-light (serde only).

use serde::{Deserialize, Serialize};

/// RFC 9457 Problem Details. The one and only error shape on every API route,
/// serialized as `application/problem+json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// Stable machine identifier for the problem class (a URI).
    #[serde(rename = "type")]
    pub r#type: String,
    /// Short, human-readable summary, stable per error class.
    pub title: String,
    /// HTTP status code, mirrored into the body.
    pub status: u16,
    /// Instance-specific human-readable explanation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Short ergonomic code clients switch on, e.g. `"BLOCK_NOT_FOUND"`.
    pub code: String,
    /// W3C Trace Context id for correlation; present on every error.
    #[serde(rename = "traceId")]
    pub trace_id: String,
    /// Per-field validation failures. Present only for multi-field errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ProblemItem>,
    /// DEPRECATED migration mirror of `detail` for legacy clients that parsed
    /// the old `{"error": "<message>"}` shape. Removed in a future release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A single field-level validation failure within [`Problem::errors`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemItem {
    /// Short ergonomic code for this specific failure.
    pub code: String,
    /// Human-readable explanation of this specific failure.
    pub detail: String,
    /// RFC 6901 JSON Pointer locating the offending field in the request body,
    /// e.g. `"/signature"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_single_error_to_rfc9457_shape() {
        let p = Problem {
            r#type: "https://docs.irys.xyz/api/errors/block-not-found".into(),
            title: "Block not found".into(),
            status: 404,
            detail: Some("Block 0xabc not found".into()),
            code: "BLOCK_NOT_FOUND".into(),
            trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
            errors: Vec::new(),
            error: None,
        };

        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v["type"],
            "https://docs.irys.xyz/api/errors/block-not-found"
        );
        assert_eq!(v["title"], "Block not found");
        assert_eq!(v["status"], 404);
        assert_eq!(v["detail"], "Block 0xabc not found");
        assert_eq!(v["code"], "BLOCK_NOT_FOUND");
        assert_eq!(v["traceId"], "0af7651916cd43dd8448eb211c80319c");

        let obj = v.as_object().unwrap();
        assert!(
            obj.get("errors").is_none(),
            "empty errors[] must be omitted"
        );
        assert!(
            obj.get("error").is_none(),
            "None error mirror must be omitted"
        );
        assert_eq!(obj.len(), 6, "only the 6 populated members must serialize");
    }

    #[test]
    fn serializes_multi_field_validation_with_pointers() {
        let p = Problem {
            r#type: "https://docs.irys.xyz/api/errors/validation".into(),
            title: "Transaction validation failed".into(),
            status: 400,
            detail: None,
            code: "VALIDATION_FAILED".into(),
            trace_id: "7c33d9f1aa20".into(),
            errors: vec![
                ProblemItem {
                    code: "INVALID_SIGNATURE".into(),
                    detail: "signature does not match signer".into(),
                    pointer: Some("/signature".into()),
                },
                ProblemItem {
                    code: "UNFUNDED".into(),
                    detail: "balance 30 < required 50".into(),
                    pointer: Some("/value".into()),
                },
            ],
            error: None,
        };

        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("detail").is_none(), "None detail must be omitted");
        assert_eq!(v["errors"].as_array().unwrap().len(), 2);
        assert_eq!(v["errors"][0]["code"], "INVALID_SIGNATURE");
        assert_eq!(v["errors"][0]["pointer"], "/signature");
        assert_eq!(v["errors"][1]["pointer"], "/value");
    }

    #[test]
    fn deserializes_from_wire_form() {
        // A client importing this type must parse the canonical wire shape.
        let wire = r#"{"type":"about:blank","title":"Block not found","status":404,
            "code":"BLOCK_NOT_FOUND","traceId":"0af7651916cd43dd8448eb211c80319c"}"#;
        let p: Problem = serde_json::from_str(wire).unwrap();
        assert_eq!(p.r#type, "about:blank");
        assert_eq!(p.code, "BLOCK_NOT_FOUND");
        assert_eq!(p.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(p.detail, None);
        assert!(p.errors.is_empty(), "missing errors[] defaults to empty");
        assert_eq!(p.error, None);
    }
}
