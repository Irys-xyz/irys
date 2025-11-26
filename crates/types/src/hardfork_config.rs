//! Configurable hardfork parameters.
//!
//! This module defines hardfork parameters that are fully configurable via TOML.
//! Each network (mainnet, testnet, devnet) can define its own parameter values
//! without requiring code changes.

use serde::{Deserialize, Serialize};

/// Configurable hardfork schedule - part of ConsensusConfig.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrysHardforkConfig {
    /// Frontier parameters (always active from genesis)
    pub frontier: FrontierParams,

    /// NextNameTBD hardfork - None means disabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_name_tbd: Option<ForkActivation>,
}

/// Parameters for Frontier hardfork (genesis defaults).
///
/// These are the parameters active from block 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierParams {
    /// Number of ingress proofs required for promotion
    pub number_of_ingress_proofs_total: u64,

    /// Number of ingress proofs required from assignees
    pub number_of_ingress_proofs_from_assignees: u64,
}

/// A hardfork activation with its parameters.
///
/// When this fork activates, the contained parameters take effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkActivation {
    /// Timestamp (seconds since epoch) at which this hardfork activates
    pub activation_timestamp: u64,

    /// Number of total ingress proofs required
    pub number_of_ingress_proofs_total: u64,

    /// Number of ingress proofs from assignees
    pub number_of_ingress_proofs_from_assignees: u64,
}

impl IrysHardforkConfig {
    /// Check if the NextNameTBD hardfork is active at a given timestamp.
    pub fn is_next_name_tbd_active(&self, timestamp: u64) -> bool {
        self.next_name_tbd
            .as_ref()
            .is_some_and(|f| timestamp >= f.activation_timestamp)
    }

    /// Get the activation timestamp for NextNameTBD hardfork, if configured.
    pub fn next_name_tbd_activation_timestamp(&self) -> Option<u64> {
        self.next_name_tbd.as_ref().map(|f| f.activation_timestamp)
    }

    /// Get the number of ingress proofs required at a specific timestamp.
    pub fn number_of_ingress_proofs_total_at(&self, timestamp: u64) -> u64 {
        if let Some(ref fork) = self.next_name_tbd {
            if timestamp >= fork.activation_timestamp {
                return fork.number_of_ingress_proofs_total;
            }
        }
        self.frontier.number_of_ingress_proofs_total
    }

    /// Get the number of ingress proofs from assignees required at a specific timestamp.
    pub fn number_of_ingress_proofs_from_assignees_at(&self, timestamp: u64) -> u64 {
        if let Some(ref fork) = self.next_name_tbd {
            if timestamp >= fork.activation_timestamp {
                return fork.number_of_ingress_proofs_from_assignees;
            }
        }
        self.frontier.number_of_ingress_proofs_from_assignees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frontier_params() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 5,
                number_of_ingress_proofs_from_assignees: 2,
            },
            next_name_tbd: None,
        };

        assert_eq!(config.number_of_ingress_proofs_total_at(0), 5);
        assert_eq!(config.number_of_ingress_proofs_from_assignees_at(0), 2);

        // Same params at any timestamp since no next fork
        assert_eq!(config.number_of_ingress_proofs_total_at(1_000_000), 5);
    }

    #[test]
    fn test_fork_activation() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 10,
                number_of_ingress_proofs_from_assignees: 0,
            },
            next_name_tbd: Some(ForkActivation {
                activation_timestamp: 1000,
                number_of_ingress_proofs_total: 4,
                number_of_ingress_proofs_from_assignees: 2,
            }),
        };

        // Before activation timestamp
        assert_eq!(config.number_of_ingress_proofs_total_at(999), 10);
        assert_eq!(config.number_of_ingress_proofs_from_assignees_at(999), 0);
        assert!(!config.is_next_name_tbd_active(999));

        // At activation timestamp
        assert_eq!(config.number_of_ingress_proofs_total_at(1000), 4);
        assert_eq!(config.number_of_ingress_proofs_from_assignees_at(1000), 2);
        assert!(config.is_next_name_tbd_active(1000));

        // After activation timestamp
        assert_eq!(config.number_of_ingress_proofs_total_at(1001), 4);
        assert_eq!(config.number_of_ingress_proofs_from_assignees_at(1001), 2);
    }

    #[test]
    fn test_toml_serialization() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 1,
                number_of_ingress_proofs_from_assignees: 0,
            },
            next_name_tbd: Some(ForkActivation {
                activation_timestamp: 5000,
                number_of_ingress_proofs_total: 4,
                number_of_ingress_proofs_from_assignees: 2,
            }),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: IrysHardforkConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn test_toml_deserialization_complete() {
        // Full config with all fields specified
        let toml_str = r#"
            [frontier]
            number_of_ingress_proofs_total = 5
            number_of_ingress_proofs_from_assignees = 2
        "#;
        let config: IrysHardforkConfig = toml::from_str(toml_str).unwrap();

        assert_eq!(config.frontier.number_of_ingress_proofs_total, 5);
        assert_eq!(config.frontier.number_of_ingress_proofs_from_assignees, 2);
        assert!(config.next_name_tbd.is_none());
    }
}
