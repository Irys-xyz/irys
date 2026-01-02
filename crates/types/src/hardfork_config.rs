//! Configurable hardfork parameters.

use crate::UnixTimestamp;
use serde::{Deserialize, Serialize};

/// Configurable hardfork schedule - part of ConsensusConfig.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrysHardforkConfig {
    /// Frontier parameters (always active from genesis)
    pub frontier: FrontierParams,

    /// NextNameTBD hardfork - None means disabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_name_tbd: Option<NextNameTBD>,

    /// Aurora hardfork - enables canonical RLP encoding on Commitment tx. None means disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aurora: Option<Aurora>,
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
pub struct NextNameTBD {
    /// Timestamp (seconds since epoch) at which this hardfork activates
    pub activation_timestamp: UnixTimestamp,

    /// Number of total ingress proofs required
    pub number_of_ingress_proofs_total: u64,

    /// Number of ingress proofs from assignees
    pub number_of_ingress_proofs_from_assignees: u64,
}

/// The Aurora hardfork deprecates V1 Commitment Transactions due to
/// nonstandard RLP encoding that caused signature errors with other language implementations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aurora {
    /// Timestamp (seconds since epoch) at which this hardfork activates
    pub activation_timestamp: UnixTimestamp,

    /// When this hardfork is activated this will be the minimum valid
    /// commitment transaction version
    pub minimum_commitment_tx_version: u8,
}

impl IrysHardforkConfig {
    /// Check if the NextNameTBD hardfork is active at a given timestamp (in seconds).
    pub fn is_next_name_tbd_active(&self, timestamp: UnixTimestamp) -> bool {
        self.next_name_tbd
            .as_ref()
            .is_some_and(|f| timestamp >= f.activation_timestamp)
    }

    /// Get the activation timestamp for NextNameTBD hardfork, if configured.
    pub fn next_name_tbd_activation_timestamp(&self) -> Option<UnixTimestamp> {
        self.next_name_tbd.as_ref().map(|f| f.activation_timestamp)
    }

    /// Get the number of ingress proofs required at a specific timestamp (in seconds).
    pub fn number_of_ingress_proofs_total_at(&self, timestamp: UnixTimestamp) -> u64 {
        if let Some(ref fork) = self.next_name_tbd {
            if timestamp >= fork.activation_timestamp {
                return fork.number_of_ingress_proofs_total;
            }
        }
        self.frontier.number_of_ingress_proofs_total
    }

    /// Get the number of ingress proofs from assignees required at a specific timestamp (in seconds).
    pub fn number_of_ingress_proofs_from_assignees_at(&self, timestamp: UnixTimestamp) -> u64 {
        if let Some(ref fork) = self.next_name_tbd {
            if timestamp >= fork.activation_timestamp {
                return fork.number_of_ingress_proofs_from_assignees;
            }
        }
        self.frontier.number_of_ingress_proofs_from_assignees
    }

    /// Get a reference to the Aurora config if active at the given timestamp.
    pub fn aurora_at(&self, timestamp: UnixTimestamp) -> Option<&Aurora> {
        self.aurora
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
    }

    /// Get the max PD chunks per block at a specific timestamp.
    /// Returns None if Sprite is not active.
    pub fn minimum_commitment_tx_version_at(&self, timestamp: UnixTimestamp) -> Option<u8> {
        self.aurora
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
            .map(|f| f.minimum_commitment_tx_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn test_frontier_params() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 5,
                number_of_ingress_proofs_from_assignees: 2,
            },
            next_name_tbd: None,
            aurora: None,
        };

        assert_eq!(
            config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0)),
            5
        );
        assert_eq!(
            config.number_of_ingress_proofs_from_assignees_at(UnixTimestamp::from_secs(0)),
            2
        );

        // Same params at any timestamp since no next fork
        assert_eq!(
            config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(1_000_000)),
            5
        );
    }

    #[test]
    fn test_aurora_params() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 5,
                number_of_ingress_proofs_from_assignees: 2,
            },
            next_name_tbd: None,
            aurora: Some(Aurora {
                activation_timestamp: UnixTimestamp::from_secs(1500),
                minimum_commitment_tx_version: 2,
            }),
        };

        // Before activation timestamp
        let aurora = config.aurora_at(UnixTimestamp::from_secs(1499));
        assert_matches!(aurora, None);

        // At activation timestamp
        let aurora = config.aurora_at(UnixTimestamp::from_secs(1500));
        assert_eq!(aurora, config.aurora.as_ref());

        // After activation timestamp
        let aurora = config.aurora_at(UnixTimestamp::from_secs(1501));
        assert_eq!(aurora, config.aurora.as_ref());
    }

    #[test]
    fn test_fork_activation() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 10,
                number_of_ingress_proofs_from_assignees: 0,
            },
            next_name_tbd: Some(NextNameTBD {
                activation_timestamp: UnixTimestamp::from_secs(1000),
                number_of_ingress_proofs_total: 4,
                number_of_ingress_proofs_from_assignees: 2,
            }),
            aurora: None,
        };

        // Before activation timestamp
        assert_eq!(
            config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(999)),
            10
        );
        assert_eq!(
            config.number_of_ingress_proofs_from_assignees_at(UnixTimestamp::from_secs(999)),
            0
        );
        assert!(!config.is_next_name_tbd_active(UnixTimestamp::from_secs(999)));

        // At activation timestamp
        assert_eq!(
            config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(1000)),
            4
        );
        assert_eq!(
            config.number_of_ingress_proofs_from_assignees_at(UnixTimestamp::from_secs(1000)),
            2
        );
        assert!(config.is_next_name_tbd_active(UnixTimestamp::from_secs(1000)));

        // After activation timestamp
        assert_eq!(
            config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(1001)),
            4
        );
        assert_eq!(
            config.number_of_ingress_proofs_from_assignees_at(UnixTimestamp::from_secs(1001)),
            2
        );
    }

    #[test]
    fn test_toml_serialization() {
        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 1,
                number_of_ingress_proofs_from_assignees: 0,
            },
            next_name_tbd: Some(NextNameTBD {
                activation_timestamp: UnixTimestamp::from_secs(5000),
                number_of_ingress_proofs_total: 4,
                number_of_ingress_proofs_from_assignees: 2,
            }),
            aurora: None,
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: IrysHardforkConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn test_toml_deserialization_complete() {
        // Full config with all fields specified
        let toml_str = "
            [frontier]
            number_of_ingress_proofs_total = 5
            number_of_ingress_proofs_from_assignees = 2
        ";
        let config: IrysHardforkConfig = toml::from_str(toml_str).unwrap();

        assert_eq!(config.frontier.number_of_ingress_proofs_total, 5);
        assert_eq!(config.frontier.number_of_ingress_proofs_from_assignees, 2);
        assert!(config.next_name_tbd.is_none());
    }
}
