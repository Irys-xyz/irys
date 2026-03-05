use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json, Path};
use irys_types::{
    serialization::string_u64, CommitmentStatus, IrysAddress, IrysTransactionId, H256, U256,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentStateResponse {
    pub address: IrysAddress,
    #[serde(with = "string_u64")]
    pub epoch_height: u64,
    pub is_staked: bool,
    pub reward_address: Option<IrysAddress>,
    pub stake: Option<StakeInfo>,
    pub pledges: Vec<PledgeInfo>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StakeInfo {
    pub id: IrysTransactionId,
    pub status: CommitmentStatus,
    pub amount: U256,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PledgeInfo {
    pub id: IrysTransactionId,
    pub status: CommitmentStatus,
    pub amount: U256,
    pub partition_hash: Option<H256>,
}

pub async fn get_commitment_state(
    address: Path<IrysAddress>,
    app_state: Data<ApiState>,
) -> Result<Json<CommitmentStateResponse>, ApiError> {
    let epoch_snapshot = app_state.block_tree.read().canonical_epoch_snapshot();
    let address_value = *address;

    let stake_entry = epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&address_value);

    let is_staked =
        stake_entry.is_some_and(|entry| entry.commitment_status == CommitmentStatus::Active);

    let reward_address = stake_entry.map(|entry| entry.reward_address);

    let stake = stake_entry.map(|entry| StakeInfo {
        id: entry.id,
        status: entry.commitment_status,
        amount: entry.amount,
    });

    let pledges = epoch_snapshot
        .commitment_state
        .pledge_commitments
        .get(&address_value)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| PledgeInfo {
                    id: entry.id,
                    status: entry.commitment_status,
                    amount: entry.amount,
                    partition_hash: entry.partition_hash,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Json(CommitmentStateResponse {
        address: address_value,
        epoch_height: epoch_snapshot.epoch_height,
        is_staked,
        reward_address,
        stake,
        pledges,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_address() -> IrysAddress {
        IrysAddress::from([0xAB; 20])
    }

    fn sample_reward_address() -> IrysAddress {
        IrysAddress::from([0xCD; 20])
    }

    fn sample_tx_id() -> IrysTransactionId {
        H256::from([0x11; 32])
    }

    fn sample_partition_hash() -> H256 {
        H256::from([0x22; 32])
    }

    #[test]
    fn test_full_response_serialization() {
        let response = CommitmentStateResponse {
            address: sample_address(),
            epoch_height: 42,
            is_staked: true,
            reward_address: Some(sample_reward_address()),
            stake: Some(StakeInfo {
                id: sample_tx_id(),
                status: CommitmentStatus::Active,
                amount: U256::from(1_000_000),
            }),
            pledges: vec![PledgeInfo {
                id: sample_tx_id(),
                status: CommitmentStatus::Pending,
                amount: U256::from(500_000),
                partition_hash: Some(sample_partition_hash()),
            }],
        };

        let json = serde_json::to_value(&response).unwrap();

        // address is base58-encoded
        assert!(json["address"].is_string());
        assert_eq!(json["address"].as_str().unwrap(), sample_address().to_string());

        // epochHeight is a string (string_u64)
        assert_eq!(json["epochHeight"], "42");

        // isStaked is a bool
        assert_eq!(json["isStaked"], true);

        // rewardAddress is base58-encoded
        assert!(json["rewardAddress"].is_string());
        assert_eq!(
            json["rewardAddress"].as_str().unwrap(),
            sample_reward_address().to_string()
        );

        // stake fields
        let stake = &json["stake"];
        assert!(stake["id"].is_string());
        assert_eq!(stake["status"], "Active");
        assert!(stake["amount"].is_string()); // U256 serializes as hex string

        // pledges
        let pledge = &json["pledges"][0];
        assert!(pledge["id"].is_string());
        assert_eq!(pledge["status"], "Pending");
        assert!(pledge["amount"].is_string());
        assert!(pledge["partitionHash"].is_string());
    }

    #[test]
    fn test_empty_response_serialization() {
        let response = CommitmentStateResponse {
            address: sample_address(),
            epoch_height: 0,
            is_staked: false,
            reward_address: None,
            stake: None,
            pledges: vec![],
        };

        let json = serde_json::to_value(&response).unwrap();

        assert_eq!(json["isStaked"], false);
        assert!(json["rewardAddress"].is_null());
        assert!(json["stake"].is_null());
        assert_eq!(json["pledges"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_roundtrip_deserialization() {
        let response = CommitmentStateResponse {
            address: sample_address(),
            epoch_height: u64::MAX,
            is_staked: true,
            reward_address: Some(sample_reward_address()),
            stake: Some(StakeInfo {
                id: sample_tx_id(),
                status: CommitmentStatus::Active,
                amount: U256::from(1_000_000),
            }),
            pledges: vec![
                PledgeInfo {
                    id: sample_tx_id(),
                    status: CommitmentStatus::Pending,
                    amount: U256::from(500_000),
                    partition_hash: Some(sample_partition_hash()),
                },
                PledgeInfo {
                    id: H256::from([0x33; 32]),
                    status: CommitmentStatus::Inactive,
                    amount: U256::from(250_000),
                    partition_hash: None,
                },
            ],
        };

        let json_str = serde_json::to_string(&response).unwrap();
        let deserialized: CommitmentStateResponse = serde_json::from_str(&json_str).unwrap();

        assert_eq!(deserialized, response);
    }

    #[test]
    fn test_commitment_status_variants_serialize_as_strings() {
        for (status, expected) in [
            (CommitmentStatus::Pending, "Pending"),
            (CommitmentStatus::Active, "Active"),
            (CommitmentStatus::Inactive, "Inactive"),
            (CommitmentStatus::Slashed, "Slashed"),
        ] {
            let info = StakeInfo {
                id: sample_tx_id(),
                status,
                amount: U256::from(0),
            };
            let json = serde_json::to_value(&info).unwrap();
            assert_eq!(json["status"], expected, "CommitmentStatus::{expected} serialization");
        }
    }
}
