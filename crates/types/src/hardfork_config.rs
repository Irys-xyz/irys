//! Configurable hardfork parameters.
//!
//! This module defines hardfork parameters that are fully configurable via TOML.
//! Each network (mainnet, testnet, devnet) can define its own parameter values
//! without requiring code changes.

use serde::{Deserialize, Serialize};

/// Runtime hardfork parameters (computed from config at a specific block height).
///
/// This struct is returned by `IrysHardforkConfig::params_at()` and contains
/// the active parameters for a given block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardforkParams {
    /// Number of storage proofs required from unique miners for promotion
    pub number_of_ingress_proofs_total: u64,
    /// Minimum proofs required from miners assigned to store the data
    pub number_of_ingress_proofs_from_assignees: u64,
}

impl Default for HardforkParams {
    /// Returns default Frontier hardfork parameters.
    fn default() -> Self {
        Self {
            number_of_ingress_proofs_total: 10,
            number_of_ingress_proofs_from_assignees: 0,
        }
    }
}

/// Configurable hardfork schedule - part of ConsensusConfig.
///
/// This struct defines all hardfork parameters in a TOML-configurable way.
/// Networks can override default values in their configuration files.
///
/// # Example TOML
///
/// ```toml
/// [consensus.hardforks.frontier]
/// number_of_ingress_proofs_total = 10
/// number_of_ingress_proofs_from_assignees = 0
///
/// # Optional: Enable next hardfork at a specific timestamp (seconds since epoch)
/// [consensus.hardforks.next_name_tbd]
/// activation_timestamp = 1735689600
/// number_of_ingress_proofs_total = 4
/// number_of_ingress_proofs_from_assignees = 2
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrysHardforkConfig {
    /// Frontier parameters (always active from genesis)
    #[serde(default)]
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
    #[serde(default = "default_proofs_total")]
    pub number_of_ingress_proofs_total: u64,

    /// Number of ingress proofs required from assignees
    #[serde(default)]
    pub number_of_ingress_proofs_from_assignees: u64,
}

impl Default for FrontierParams {
    fn default() -> Self {
        Self {
            number_of_ingress_proofs_total: default_proofs_total(),
            number_of_ingress_proofs_from_assignees: 0,
        }
    }
}

fn default_proofs_total() -> u64 {
    10
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
    /// Get hardfork parameters at a specific timestamp.
    ///
    /// This checks hardforks from newest to oldest, returning parameters
    /// for the most recent active hardfork.
    pub fn params_at(&self, timestamp: u64) -> HardforkParams {
        // Check newest fork first
        if let Some(ref fork) = self.next_name_tbd {
            if timestamp >= fork.activation_timestamp {
                return HardforkParams {
                    number_of_ingress_proofs_total: fork.number_of_ingress_proofs_total,
                    number_of_ingress_proofs_from_assignees: fork
                        .number_of_ingress_proofs_from_assignees,
                };
            }
        }

        // Default to frontier
        HardforkParams {
            number_of_ingress_proofs_total: self.frontier.number_of_ingress_proofs_total,
            number_of_ingress_proofs_from_assignees: self
                .frontier
                .number_of_ingress_proofs_from_assignees,
        }
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_params() {
        let config = IrysHardforkConfig::default();
        let params = config.params_at(0);

        assert_eq!(params.number_of_ingress_proofs_total, 10);
        assert_eq!(params.number_of_ingress_proofs_from_assignees, 0);
    }

    #[test]
    fn test_custom_frontier_params() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 5,
                number_of_ingress_proofs_from_assignees: 2,
            },
            next_name_tbd: None,
        };

        let params = config.params_at(0);
        assert_eq!(params.number_of_ingress_proofs_total, 5);
        assert_eq!(params.number_of_ingress_proofs_from_assignees, 2);

        // Same params at any timestamp since no next fork
        let params = config.params_at(1_000_000);
        assert_eq!(params.number_of_ingress_proofs_total, 5);
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
        let params = config.params_at(999);
        assert_eq!(params.number_of_ingress_proofs_total, 10);
        assert_eq!(params.number_of_ingress_proofs_from_assignees, 0);
        assert!(!config.is_next_name_tbd_active(999));

        // At activation timestamp
        let params = config.params_at(1000);
        assert_eq!(params.number_of_ingress_proofs_total, 4);
        assert_eq!(params.number_of_ingress_proofs_from_assignees, 2);
        assert!(config.is_next_name_tbd_active(1000));

        // After activation timestamp
        let params = config.params_at(1001);
        assert_eq!(params.number_of_ingress_proofs_total, 4);
        assert_eq!(params.number_of_ingress_proofs_from_assignees, 2);
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
    fn test_toml_deserialization_with_defaults() {
        // Empty config should use defaults
        let toml_str = "";
        let config: IrysHardforkConfig = toml::from_str(toml_str).unwrap();

        assert_eq!(config.frontier.number_of_ingress_proofs_total, 10);
        assert_eq!(config.frontier.number_of_ingress_proofs_from_assignees, 0);
        assert!(config.next_name_tbd.is_none());
    }

    #[test]
    fn test_toml_deserialization_partial() {
        // Only override frontier proofs_total
        let toml_str = r#"
            [frontier]
            number_of_ingress_proofs_total = 5
        "#;
        let config: IrysHardforkConfig = toml::from_str(toml_str).unwrap();

        assert_eq!(config.frontier.number_of_ingress_proofs_total, 5);
        assert_eq!(config.frontier.number_of_ingress_proofs_from_assignees, 0); // default
        assert!(config.next_name_tbd.is_none());
    }
}
