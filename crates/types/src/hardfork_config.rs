//! Configurable hardfork parameters.

use crate::storage_pricing::{phantoms::Usd, Amount};
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

    /// Sprite hardfork - enables Programmable Data features. None means disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprite: Option<Sprite>,
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
    /// Timestamp at which this hardfork activates
    pub activation_timestamp: UnixTimestamp,

    /// Number of total ingress proofs required
    pub number_of_ingress_proofs_total: u64,

    /// Number of ingress proofs from assignees
    pub number_of_ingress_proofs_from_assignees: u64,
}

/// Sprite hardfork - enables Programmable Data (PD) features.
///
/// When this fork activates, PD transactions become valid and the PD precompile is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Get the minimum PD transaction cost in USD at a specific timestamp.
    /// Returns None if Sprite is not active.
    pub fn min_pd_transaction_cost_at(&self, timestamp: UnixTimestamp) -> Option<Amount<Usd>> {
        self.sprite
            .as_ref()
            .filter(|f| timestamp >= f.activation_timestamp)
            .map(|f| f.min_pd_transaction_cost)
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
            sprite: None,
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
}
