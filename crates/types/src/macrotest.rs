// random tests from claude
// TODO: remove
use arbitrary::Arbitrary;
use irys_macros_integer_tagged::IntegerTagged;
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

// Example 1: IngressProof with "version" tag
#[derive(
    Debug,
    Clone,
    PartialEq,
    Default,
    Eq,
    Serialize,
    Deserialize,
    Compact,
    Arbitrary,
    alloy_rlp::RlpEncodable,
    alloy_rlp::RlpDecodable,
)]
pub struct IngressProofV1 {
    pub timestamp: u64,
    pub data: String,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Default,
    Eq,
    Serialize,
    Deserialize,
    Compact,
    Arbitrary,
    alloy_rlp::RlpEncodable,
    alloy_rlp::RlpDecodable,
)]
pub struct IngressProofV2 {
    pub timestamp: u64,
    pub data2: String,
}

#[derive(Debug, Clone, PartialEq, Eq, IntegerTagged, Compact, Arbitrary)]
#[integer_tagged(tag = "version")]
pub enum IngressProof {
    #[integer_tagged(version = 1)]
    V1(IngressProofV1),
    #[integer_tagged(version = 2)]
    V2(IngressProofV2),
}

impl Default for IngressProof {
    fn default() -> Self {
        Self::V1(Default::default())
    }
}

// Example 2: Config with custom tag name
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigV1 {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigV2 {
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, IntegerTagged)]
#[integer_tagged(tag = "config_version")]
pub enum Config {
    #[integer_tagged(version = 1)]
    V1(ConfigV1),
    #[integer_tagged(version = 2)]
    V2(ConfigV2),
}

// Example 3: Using default tag name "version"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadV1 {
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, IntegerTagged)]
// No tag attribute means it defaults to "version"
pub enum Payload {
    #[integer_tagged(version = 1)]
    V1(PayloadV1),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ingress_proof_serialization() {
        let proof = IngressProof::V1(IngressProofV1 {
            data: "test".to_string(),
            timestamp: 1234567890,
        });

        let json = serde_json::to_string(&proof).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Version is an integer, not a string
        assert_eq!(value["version"], 1);
        assert_eq!(value["data"], "test");
        assert_eq!(value["timestamp"], 1234567890);
    }

    #[test]
    fn test_ingress_proof_deserialization() {
        let json = r#"{"version": 2, "data2": "test", "timestamp": 1234567890}"#;
        let proof: IngressProof = serde_json::from_str(json).unwrap();

        match proof {
            IngressProof::V2(v2) => {
                assert_eq!(v2.data2, "test");
                assert_eq!(v2.timestamp, 1234567890);
            }
            _ => panic!("Expected V2 variant"),
        }
    }

    #[test]
    fn test_config_with_custom_tag() {
        let config = Config::V2(ConfigV2 {
            name: "my_config".to_string(),
            enabled: true,
        });

        let json = serde_json::to_string(&config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Uses custom tag name "config_version"
        assert_eq!(value["config_version"], 2);
        assert_eq!(value["name"], "my_config");
        assert_eq!(value["enabled"], true);
    }

    #[test]
    fn test_roundtrip() {
        let original = IngressProof::V1(IngressProofV1 {
            data: "roundtrip".to_string(),
            timestamp: 9876543210,
        });

        let json = serde_json::to_string(&original).unwrap();
        let deserialized: IngressProof = serde_json::from_str(&json).unwrap();

        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_missing_version_error() {
        let json = r#"{"data": "test", "timestamp": 1234567890}"#;
        let result: Result<IngressProof, _> = serde_json::from_str(json);

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("version") || error.contains("missing field"));
    }

    #[test]
    fn test_unknown_version_error() {
        let json = r#"{"version": 999, "data": "test"}"#;
        let result: Result<IngressProof, _> = serde_json::from_str(json);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unknown version: 999"));
    }
}
