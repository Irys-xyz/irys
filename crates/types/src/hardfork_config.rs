//! Configurable hardfork parameters.

use crate::storage_pricing::{phantoms::Usd, Amount};
use crate::{serialization::unix_timestamp_string_serde, UnixTimestamp, VersionDiscriminant};
use serde::{Deserialize, Serialize};

/// Configurable hardfork schedule - part of ConsensusConfig.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IrysHardforkConfig {
    /// Frontier parameters (always active from genesis)
    pub frontier: FrontierParams,

    /// NextNameTBD hardfork - None means disabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_name_tbd: Option<NextNameTBD>,

    /// Sprite hardfork - enables Programmable Data features. None means disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprite: Option<Sprite>,

    /// Aurora hardfork - enables canonical RLP encoding on Commitment tx. None means disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aurora: Option<Aurora>,

    /// Borealis hardfork - enables UpdateRewardAddress commitment transactions.
    /// Activation is epoch-aligned: enabled for all blocks in an epoch if the epoch block's
    /// timestamp >= activation_timestamp. None means disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub borealis: Option<Borealis>,
}

/// Parameters for Frontier hardfork (genesis defaults).
///
/// These are the parameters active from block 0.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrontierParams {
    /// Number of ingress proofs required for promotion
    pub number_of_ingress_proofs_total: u64,

    /// Number of ingress proofs required from assignees
    pub number_of_ingress_proofs_from_assignees: u64,
}

/// A hardfork activation with its parameters.
///
/// When this fork activates, the contained parameters take effect.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NextNameTBD {
    /// Times (seconds since epoch) at which this hardfork activates
    #[serde(with = "unix_timestamp_string_serde")]
    pub activation_timestamp: UnixTimestamp,

    /// Number of total ingress proofs required
    pub number_of_ingress_proofs_total: u64,

    /// Number of ingress proofs from assignees
    pub number_of_ingress_proofs_from_assignees: u64,
}

/// Sprite hardfork - enables Programmable Data (PD) features.
///
/// When this fork activates, PD transactions become valid and the PD precompile is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Sprite {
    /// Timestamp at which this hardfork activates
    pub activation_timestamp: UnixTimestamp,

    /// Cost per 1MB of Programmable Data in USD.
    /// Used as the initial/target base fee for PD pricing.
    pub cost_per_mb: Amount<Usd>,

    /// Floor for base fee - base fee cannot drop below this value.
    /// Expressed as USD per MB.
    pub base_fee_floor: Amount<Usd>,

    /// Maximum number of PD chunks that can be included in a single block.
    /// This limit prevents exceeding the network's chunk processing capacity per block.
    pub max_pd_chunks_per_block: u64,

    /// Minimum cost for a PD transaction in USD.
    /// Transactions with total PD cost (base_fee + priority_fee) × chunks below this
    /// threshold will be rejected. This prevents spam and ensures economic viability.
    /// Expressed in USD (1e18 scale).
    pub min_pd_transaction_cost: Amount<Usd>,
}

/// Aurora hardfork - enables canonical RLP encoding on Commitment tx.
///
/// When this fork activates, commitment transactions must use the minimum version
/// specified for proper canonical encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Aurora {
    /// Timestamp (seconds since epoch) at which this hardfork activates
    #[serde(with = "unix_timestamp_string_serde")]
    pub activation_timestamp: UnixTimestamp,
    /// Minimum version required for commitment transactions
    pub minimum_commitment_tx_version: u8,
}

/// Borealis hardfork - enables UpdateRewardAddress commitment transactions.
///
/// Activation is epoch-aligned: the feature is enabled for all blocks in an epoch
/// if that epoch's epoch block has a timestamp >= activation_timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Borealis {
    /// Timestamp (seconds since epoch) at which this hardfork activates.
    /// The actual activation happens at the first epoch boundary where the
    /// epoch block's timestamp meets or exceeds this value.
    #[serde(with = "unix_timestamp_string_serde")]
    pub activation_timestamp: UnixTimestamp,
}

impl IrysHardforkConfig {
    /// Check if the NextNameTBD hardfork is active at a given timestamp.
    pub fn is_next_name_tbd_active(&self, timestamp: UnixTimestamp) -> bool {
        self.next_name_tbd
            .as_ref()
            .is_some_and(|f| timestamp >= f.activation_timestamp)
    }

    /// Get the activation timestamp for NextNameTBD hardfork, if configured.
    pub fn next_name_tbd_activation_timestamp(&self) -> Option<UnixTimestamp> {
        self.next_name_tbd.as_ref().map(|f| f.activation_timestamp)
    }

    /// Get the number of ingress proofs required at a specific timestamp.
    pub fn number_of_ingress_proofs_total_at(&self, timestamp: UnixTimestamp) -> u64 {
        if let Some(ref fork) = self.next_name_tbd {
            if timestamp >= fork.activation_timestamp {
                return fork.number_of_ingress_proofs_total;
            }
        }
        self.frontier.number_of_ingress_proofs_total
    }

    /// Get the number of ingress proofs from assignees required at a specific timestamp.
    pub fn number_of_ingress_proofs_from_assignees_at(&self, timestamp: UnixTimestamp) -> u64 {
        if let Some(ref fork) = self.next_name_tbd {
            if timestamp >= fork.activation_timestamp {
                return fork.number_of_ingress_proofs_from_assignees;
            }
        }
        self.frontier.number_of_ingress_proofs_from_assignees
    }

    /// Check if the Sprite hardfork is active at a given timestamp.
    pub fn is_sprite_active(&self, timestamp: UnixTimestamp) -> bool {
        self.sprite
            .as_ref()
            .is_some_and(|f| timestamp >= f.activation_timestamp)
    }

    /// Get the activation timestamp for Sprite hardfork, if configured.
    pub fn sprite_activation_timestamp(&self) -> Option<UnixTimestamp> {
        self.sprite.as_ref().map(|f| f.activation_timestamp)
    }

    /// Get the max PD chunks per block at a specific timestamp.
    /// Returns None if Sprite is not active.
    pub fn max_pd_chunks_per_block_at(&self, timestamp: UnixTimestamp) -> Option<u64> {
        self.sprite
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
            .map(|f| f.max_pd_chunks_per_block)
    }

    /// Get the PD cost per MB at a specific timestamp.
    /// Returns None if Sprite is not active.
    pub fn pd_cost_per_mb_at(&self, timestamp: UnixTimestamp) -> Option<Amount<Usd>> {
        self.sprite
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
            .map(|f| f.cost_per_mb)
    }

    /// Get the PD base fee floor at a specific timestamp.
    /// Returns None if Sprite is not active.
    pub fn pd_base_fee_floor_at(&self, timestamp: UnixTimestamp) -> Option<Amount<Usd>> {
        self.sprite
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
            .map(|f| f.base_fee_floor)
    }

    /// Get a reference to the Sprite config if active at the given timestamp.
    pub fn sprite_at(&self, timestamp: UnixTimestamp) -> Option<&Sprite> {
        self.sprite
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
    }

    /// Get a reference to the Aurora config if active at the given timestamp.
    #[must_use]
    pub fn aurora_at(&self, timestamp: UnixTimestamp) -> Option<&Aurora> {
        self.aurora
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
    }

    /// Get the minimum PD transaction cost in USD at a specific timestamp.
    /// Returns None if Sprite is not active.
    pub fn min_pd_transaction_cost_at(&self, timestamp: UnixTimestamp) -> Option<Amount<Usd>> {
        self.sprite
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
            .map(|f| f.min_pd_transaction_cost)
    }

    /// Check if a commitment transaction version is valid at a given timestamp.
    /// Returns true if no hardfork is active or if version meets minimum requirement.
    #[must_use]
    pub fn is_commitment_version_valid(&self, version: u8, timestamp: UnixTimestamp) -> bool {
        self.aurora_at(timestamp)
            .is_none_or(|aurora| version >= aurora.minimum_commitment_tx_version)
    }

    /// Get the minimum required commitment transaction version at a given timestamp.
    /// Returns None if no version requirement is active.
    #[must_use]
    pub fn minimum_commitment_version_at(&self, timestamp: UnixTimestamp) -> Option<u8> {
        self.aurora_at(timestamp)
            .map(|aurora| aurora.minimum_commitment_tx_version)
    }

    /// Retain only commitment transactions that meet the minimum version requirement.
    /// Removes transactions that don't meet the minimum version requirement at the given timestamp.
    pub fn retain_valid_commitment_versions<T: VersionDiscriminant>(
        &self,
        commitments: &mut Vec<T>,
        timestamp: UnixTimestamp,
    ) {
        if let Some(min_version) = self.minimum_commitment_version_at(timestamp) {
            commitments.retain(|tx| tx.version() >= min_version);
        }
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
            sprite: None,
            aurora: None,
            borealis: None,
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
            sprite: None,
            aurora: Some(Aurora {
                activation_timestamp: UnixTimestamp::from_secs(1500),
                minimum_commitment_tx_version: 2,
            }),
            borealis: None,
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
            sprite: None,
            aurora: None,
            borealis: None,
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
            sprite: None,
            aurora: None,
            borealis: None,
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
        assert!(config.sprite.is_none());
        assert!(config.aurora.is_none());
    }

    #[test]
    fn test_sprite_activation() {
        use crate::U256;

        let config = IrysHardforkConfig {
            frontier: FrontierParams {
                number_of_ingress_proofs_total: 1,
                number_of_ingress_proofs_from_assignees: 0,
            },
            next_name_tbd: None,
            sprite: Some(Sprite {
                activation_timestamp: UnixTimestamp::from_secs(2000),
                cost_per_mb: Amount::new(U256::from(100_000)),
                base_fee_floor: Amount::new(U256::from(10_000)),
                max_pd_chunks_per_block: 7500,
                min_pd_transaction_cost: Amount::new(U256::from(10_000_000_000_000_000_u64)), // 0.01 USD
            }),
            aurora: None,
            borealis: None,
        };

        // Before activation
        assert!(!config.is_sprite_active(UnixTimestamp::from_secs(1999)));
        assert!(config
            .max_pd_chunks_per_block_at(UnixTimestamp::from_secs(1999))
            .is_none());
        assert!(config
            .pd_cost_per_mb_at(UnixTimestamp::from_secs(1999))
            .is_none());
        assert!(config
            .pd_base_fee_floor_at(UnixTimestamp::from_secs(1999))
            .is_none());
        assert!(config.sprite_at(UnixTimestamp::from_secs(1999)).is_none());
        assert!(config
            .min_pd_transaction_cost_at(UnixTimestamp::from_secs(1999))
            .is_none());

        // At activation
        assert!(config.is_sprite_active(UnixTimestamp::from_secs(2000)));
        assert_eq!(
            config.max_pd_chunks_per_block_at(UnixTimestamp::from_secs(2000)),
            Some(7500)
        );
        assert_eq!(
            config.pd_cost_per_mb_at(UnixTimestamp::from_secs(2000)),
            Some(Amount::new(U256::from(100_000)))
        );
        assert_eq!(
            config.pd_base_fee_floor_at(UnixTimestamp::from_secs(2000)),
            Some(Amount::new(U256::from(10_000)))
        );
        assert!(config.sprite_at(UnixTimestamp::from_secs(2000)).is_some());
        assert_eq!(
            config.min_pd_transaction_cost_at(UnixTimestamp::from_secs(2000)),
            Some(Amount::new(U256::from(10_000_000_000_000_000_u64)))
        );

        // After activation
        assert!(config.is_sprite_active(UnixTimestamp::from_secs(3000)));
        assert_eq!(
            config.max_pd_chunks_per_block_at(UnixTimestamp::from_secs(3000)),
            Some(7500)
        );
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
                sprite: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_ts),
                    minimum_commitment_tx_version: min_version,
                }),
                borealis: None,
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
                sprite: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_ts),
                    minimum_commitment_tx_version: min_version,
                }),
                borealis: None,
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
                sprite: None,
                aurora: None,
                borealis: None,
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
                sprite: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_ts),
                    minimum_commitment_tx_version: min_version,
                }),
                borealis: None,
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

    mod commitment_version_helpers {
        use super::*;
        use rstest::rstest;

        fn config_with_aurora(activation_secs: u64, min_version: u8) -> IrysHardforkConfig {
            IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                sprite: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_secs),
                    minimum_commitment_tx_version: min_version,
                }),
                borealis: None,
            }
        }

        fn config_without_aurora() -> IrysHardforkConfig {
            IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                sprite: None,
                aurora: None,
                borealis: None,
            }
        }

        #[rstest]
        #[case::before_activation_v1_valid(999, 1, true)]
        #[case::before_activation_v2_valid(999, 2, true)]
        #[case::at_activation_v1_invalid(1000, 1, false)]
        #[case::at_activation_v2_valid(1000, 2, true)]
        #[case::after_activation_v1_invalid(1001, 1, false)]
        #[case::after_activation_v2_valid(1001, 2, true)]
        #[case::after_activation_v3_valid(1001, 3, true)]
        fn test_is_commitment_version_valid(
            #[case] timestamp_secs: u64,
            #[case] version: u8,
            #[case] expected_valid: bool,
        ) {
            let config = config_with_aurora(1000, 2);
            assert_eq!(
                config
                    .is_commitment_version_valid(version, UnixTimestamp::from_secs(timestamp_secs)),
                expected_valid
            );
        }

        #[rstest]
        #[case::v1_always_valid_without_aurora(0, 1, true)]
        #[case::v1_valid_at_max_time(u64::MAX, 1, true)]
        fn test_is_commitment_version_valid_no_aurora(
            #[case] timestamp_secs: u64,
            #[case] version: u8,
            #[case] expected_valid: bool,
        ) {
            let config = config_without_aurora();
            assert_eq!(
                config
                    .is_commitment_version_valid(version, UnixTimestamp::from_secs(timestamp_secs)),
                expected_valid
            );
        }

        #[rstest]
        #[case::before_activation_no_minimum(999, None)]
        #[case::at_activation_minimum_2(1000, Some(2))]
        #[case::after_activation_minimum_2(1001, Some(2))]
        fn test_minimum_commitment_version_at(
            #[case] timestamp_secs: u64,
            #[case] expected: Option<u8>,
        ) {
            let config = config_with_aurora(1000, 2);
            assert_eq!(
                config.minimum_commitment_version_at(UnixTimestamp::from_secs(timestamp_secs)),
                expected
            );
        }
    }
}
