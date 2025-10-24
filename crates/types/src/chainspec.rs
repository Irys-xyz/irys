//! Chainspec-related types for Irys.

use std::collections::BTreeMap;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IrysHardforksInConfig {
    // Frontier hardfork is always enabled by default, just like in Ethereum.
    //
    // Add new hardforks like this:
    // ```rust
    // ForkName: ForkCondition
    // ````
}

impl From<IrysHardforksInConfig> for BTreeMap<String, serde_json::Value> {
    fn from(val: IrysHardforksInConfig) -> Self {
        let serialized = serde_json::to_value(val)
            .expect("IrysHardforksInConfig must serialize to a JSON object");

        match serialized {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => {
                panic!(
                    "IrysHardforksInConfig should serialize to a JSON object so it can be stored in OtherFields"
                );
            }
        }
    }
}
