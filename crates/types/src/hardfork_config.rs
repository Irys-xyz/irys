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

/// Aurora deprecates V1 Commitment Transactions due to nonstandard RLP encoding
/// that caused signature errors with other language implementations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aurora {
    pub activation_timestamp: UnixTimestamp,
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

    #[must_use]
    pub fn aurora_at(&self, timestamp: UnixTimestamp) -> Option<&Aurora> {
        self.aurora
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use proptest::prelude::*;

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

    proptest! {
        /// Property: aurora_at returns None for timestamps before activation
        #[test]
        fn aurora_inactive_before_activation(
            activation_ts in 1_u64..u64::MAX,
            query_offset in 1_u64..10000_u64,
            min_version in 1_u8..=255_u8,
        ) {
            let query_ts = activation_ts.saturating_sub(query_offset);
            // Skip if query_ts >= activation_ts (would be at or after activation)
            prop_assume!(query_ts < activation_ts);

            let config = IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_ts),
                    minimum_commitment_tx_version: min_version,
                }),
            };

            prop_assert!(config.aurora_at(UnixTimestamp::from_secs(query_ts)).is_none());
        }

        /// Property: aurora_at returns Some for timestamps at or after activation
        #[test]
        fn aurora_active_at_and_after_activation(
            activation_ts in 0_u64..u64::MAX / 2,
            query_offset in 0_u64..10000_u64,
            min_version in 1_u8..=255_u8,
        ) {
            let query_ts = activation_ts.saturating_add(query_offset);

            let config = IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_ts),
                    minimum_commitment_tx_version: min_version,
                }),
            };

            let result = config.aurora_at(UnixTimestamp::from_secs(query_ts));
            prop_assert!(result.is_some());
            prop_assert_eq!(result.unwrap().minimum_commitment_tx_version, min_version);
        }

        /// Property: aurora_at always returns None when aurora is not configured
        #[test]
        fn aurora_disabled_always_none(query_ts in 0_u64..u64::MAX) {
            let config = IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                aurora: None,
            };

            prop_assert!(config.aurora_at(UnixTimestamp::from_secs(query_ts)).is_none());
        }

        /// Property: version validation is correct across boundaries
        /// V1 (version=1) should be rejected when aurora is active and min_version >= 2
        #[test]
        fn version_validation_property(
            activation_ts in 1000_u64..u64::MAX / 2,
            time_offset in 0_i64..2000_i64,
            tx_version in 1_u8..=3_u8,
            min_version in 1_u8..=3_u8,
        ) {
            let query_ts = if time_offset >= 0 {
                activation_ts.saturating_add(time_offset as u64)
            } else {
                activation_ts.saturating_sub(time_offset.unsigned_abs())
            };

            let config = IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_ts),
                    minimum_commitment_tx_version: min_version,
                }),
            };

            let aurora = config.aurora_at(UnixTimestamp::from_secs(query_ts));
            let is_active = query_ts >= activation_ts;
            let version_accepted = aurora.is_none_or(|a| tx_version >= a.minimum_commitment_tx_version);

            // If aurora is active, version must meet minimum; otherwise always accepted
            if is_active {
                prop_assert_eq!(version_accepted, tx_version >= min_version);
            } else {
                prop_assert!(version_accepted);
            }
        }
    }
}
