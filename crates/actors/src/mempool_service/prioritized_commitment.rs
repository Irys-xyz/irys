use irys_primitives::CommitmentType;
use irys_types::{CommitmentTransaction, IrysTransactionCommon};

/// Wrapper for sorting commitments by priority
/// 
/// The ordering is:
/// 1. Stake commitments (sorted by fee, highest first)
/// 2. Pledge commitments (sorted by pledge_count_before_executing ascending, then by fee descending)
/// 3. Other commitment types (sorted by fee)
#[derive(Clone, Debug)]
pub struct PrioritizedCommitment(pub CommitmentTransaction);

impl Ord for PrioritizedCommitment {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // First, compare by commitment type (Stake > Pledge/Unpledge)
        match (&self.0.commitment_type, &other.0.commitment_type) {
            (CommitmentType::Stake, CommitmentType::Stake) => {
                // Both are stakes, sort by fee (higher first)
                other.0.user_fee().cmp(&self.0.user_fee())
            }
            (CommitmentType::Stake, _) => std::cmp::Ordering::Less, // Stake comes first
            (_, CommitmentType::Stake) => std::cmp::Ordering::Greater, // Stake comes first
            (
                CommitmentType::Pledge {
                    pledge_count_before_executing: count_a,
                },
                CommitmentType::Pledge {
                    pledge_count_before_executing: count_b,
                },
            ) => {
                // Both are pledges, sort by count (lower first), then by fee
                match count_a.cmp(count_b) {
                    std::cmp::Ordering::Equal => {
                        // Same count, sort by fee (higher first)
                        other.0.user_fee().cmp(&self.0.user_fee())
                    }
                    ordering => ordering,
                }
            }
            // Handle other cases (Unpledge, Unstake) - sort by fee
            _ => other.0.user_fee().cmp(&self.0.user_fee()),
        }
    }
}

impl PartialOrd for PrioritizedCommitment {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for PrioritizedCommitment {}

impl PartialEq for PrioritizedCommitment {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_signer::Signature;
    use irys_types::{Address, H256, IrysSignature, U256};

    fn create_test_commitment(
        id: &str,
        commitment_type: CommitmentType,
        fee: u64,
    ) -> CommitmentTransaction {
        CommitmentTransaction {
            id: H256::from_slice(&[id.as_bytes()[0]; 32]),
            anchor: H256::zero(),
            signer: Address::default(),
            signature: IrysSignature::new(Signature::test_signature()),
            fee,
            value: U256::zero(),
            commitment_type,
            version: 1,
            chain_id: 1,
        }
    }

    #[test]
    fn test_stake_comes_before_pledge() {
        let stake = PrioritizedCommitment(create_test_commitment("stake", CommitmentType::Stake, 100));
        let pledge = PrioritizedCommitment(create_test_commitment(
            "pledge",
            CommitmentType::Pledge {
                pledge_count_before_executing: 1,
            },
            200,
        ));

        assert!(stake < pledge);
    }

    #[test]
    fn test_stake_sorted_by_fee() {
        let stake_low = PrioritizedCommitment(create_test_commitment("stake1", CommitmentType::Stake, 50));
        let stake_high = PrioritizedCommitment(create_test_commitment("stake2", CommitmentType::Stake, 150));

        assert!(stake_high < stake_low);
    }

    #[test]
    fn test_pledge_sorted_by_count_then_fee() {
        let pledge_count2_fee100 = PrioritizedCommitment(create_test_commitment(
            "p1",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            100,
        ));
        let pledge_count2_fee200 = PrioritizedCommitment(create_test_commitment(
            "p2",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            200,
        ));
        let pledge_count5_fee300 = PrioritizedCommitment(create_test_commitment(
            "p3",
            CommitmentType::Pledge {
                pledge_count_before_executing: 5,
            },
            300,
        ));

        // Lower count comes first
        assert!(pledge_count2_fee100 < pledge_count5_fee300);
        assert!(pledge_count2_fee200 < pledge_count5_fee300);

        // Same count, higher fee comes first
        assert!(pledge_count2_fee200 < pledge_count2_fee100);
    }

    #[test]
    fn test_complete_ordering() {
        let mut commitments = vec![
            PrioritizedCommitment(create_test_commitment(
                "pledge5",
                CommitmentType::Pledge {
                    pledge_count_before_executing: 5,
                },
                100,
            )),
            PrioritizedCommitment(create_test_commitment("stake50", CommitmentType::Stake, 50)),
            PrioritizedCommitment(create_test_commitment(
                "pledge2_200",
                CommitmentType::Pledge {
                    pledge_count_before_executing: 2,
                },
                200,
            )),
            PrioritizedCommitment(create_test_commitment("stake150", CommitmentType::Stake, 150)),
            PrioritizedCommitment(create_test_commitment(
                "pledge2_50",
                CommitmentType::Pledge {
                    pledge_count_before_executing: 2,
                },
                50,
            )),
            PrioritizedCommitment(create_test_commitment(
                "pledge10",
                CommitmentType::Pledge {
                    pledge_count_before_executing: 10,
                },
                300,
            )),
            PrioritizedCommitment(create_test_commitment("unstake", CommitmentType::Unstake, 75)),
        ];

        commitments.sort();

        // Verify the expected order
        let ids: Vec<_> = commitments
            .iter()
            .map(|c| format!("{:02x}", c.0.id.as_bytes()[0]))
            .collect();

        // Expected order:
        // 1. stake150 (Stake, higher fee)
        // 2. stake50 (Stake, lower fee)
        // 3. pledge2_200 (Pledge count=2, fee=200)
        // 4. pledge2_50 (Pledge count=2, fee=50)
        // 5. pledge5 (Pledge count=5)
        // 6. pledge10 (Pledge count=10)
        // 7. unstake (Other type)
        assert_eq!(ids[0], format!("{:02x}", b's')); // stake150
        assert_eq!(ids[1], format!("{:02x}", b's')); // stake50
        assert_eq!(ids[2], format!("{:02x}", b'p')); // pledge2_200
        assert_eq!(ids[3], format!("{:02x}", b'p')); // pledge2_50
        assert_eq!(ids[4], format!("{:02x}", b'p')); // pledge5
        assert_eq!(ids[5], format!("{:02x}", b'p')); // pledge10
        assert_eq!(ids[6], format!("{:02x}", b'u')); // unstake
    }

    #[test]
    fn test_equality_based_on_id() {
        let commitment1 = PrioritizedCommitment(create_test_commitment("same", CommitmentType::Stake, 100));
        let mut commitment2 = create_test_commitment("same", CommitmentType::Stake, 200);
        commitment2.id = commitment1.0.id; // Make IDs the same

        let prioritized2 = PrioritizedCommitment(commitment2);

        assert_eq!(commitment1, prioritized2);
    }
}