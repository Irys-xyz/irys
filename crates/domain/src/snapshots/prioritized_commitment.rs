use irys_primitives::CommitmentType;
use irys_types::{CommitmentTransaction, IrysTransactionCommon as _};
use std::borrow::Borrow;

/// Wrapper for sorting commitments by priority
///
/// The ordering is:
/// 1. Stake commitments (sorted by fee, highest first)
/// 2. Pledge commitments (sorted by pledge_count_before_executing ascending, then by fee descending)
/// 3. Other commitment types (sorted by fee)
#[derive(Debug)]
pub struct PrioritizedCommitment<T: Borrow<CommitmentTransaction>>(pub T);

impl<T: Borrow<CommitmentTransaction>> Ord for PrioritizedCommitment<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let this = self.0.borrow();
        let other = other.0.borrow();
        // First, compare by commitment type (Stake > Pledge/Unpledge)
        match (&this.commitment_type, &other.commitment_type) {
            (CommitmentType::Stake, CommitmentType::Stake) => {
                // Both are stakes, sort by fee (higher first)
                other.user_fee().cmp(&this.user_fee())
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
                        other.user_fee().cmp(&this.user_fee())
                    }
                    ordering => ordering,
                }
            }
            // Handle other cases (Unpledge, Unstake) - sort by fee
            _ => other.user_fee().cmp(&this.user_fee()),
        }
    }
}

impl<T: Borrow<CommitmentTransaction>> PartialOrd for PrioritizedCommitment<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Borrow<CommitmentTransaction>> Eq for PrioritizedCommitment<T> {}

impl<T: Borrow<CommitmentTransaction>> PartialEq for PrioritizedCommitment<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.borrow().id == other.0.borrow().id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{Address, IrysSignature, H256, U256};

    fn create_test_commitment(
        id: &str,
        commitment_type: CommitmentType,
        fee: u64,
    ) -> CommitmentTransaction {
        CommitmentTransaction {
            id: H256::from_slice(&[id.as_bytes()[0]; 32]),
            anchor: H256::zero(),
            signer: Address::default(),
            signature: IrysSignature::default(),
            fee,
            value: U256::zero(),
            commitment_type,
            version: 1,
            chain_id: 1,
        }
    }

    #[test]
    fn test_stake_comes_before_pledge() {
        let stake = create_test_commitment("stake", CommitmentType::Stake, 100);
        let pledge = create_test_commitment(
            "pledge",
            CommitmentType::Pledge {
                pledge_count_before_executing: 1,
            },
            200,
        );
        let stake_ref = PrioritizedCommitment(&stake);
        let pledge_ref = PrioritizedCommitment(&pledge);

        assert!(stake_ref < pledge_ref);
    }

    #[test]
    fn test_stake_sorted_by_fee() {
        let stake_low = create_test_commitment("stake1", CommitmentType::Stake, 50);
        let stake_high = create_test_commitment("stake2", CommitmentType::Stake, 150);
        let stake_low_ref = PrioritizedCommitment(&stake_low);
        let stake_high_ref = PrioritizedCommitment(&stake_high);

        assert!(stake_high_ref < stake_low_ref);
    }

    #[test]
    fn test_pledge_sorted_by_count_then_fee() {
        let pledge_count2_fee100 = create_test_commitment(
            "p1",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            100,
        );
        let pledge_count2_fee200 = create_test_commitment(
            "p2",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            200,
        );
        let pledge_count5_fee300 = create_test_commitment(
            "p3",
            CommitmentType::Pledge {
                pledge_count_before_executing: 5,
            },
            300,
        );

        let ref1 = PrioritizedCommitment(&pledge_count2_fee100);
        let ref2 = PrioritizedCommitment(&pledge_count2_fee200);
        let ref3 = PrioritizedCommitment(&pledge_count5_fee300);

        // Lower count comes first
        assert!(ref1 < ref3);
        assert!(ref2 < ref3);

        // Same count, higher fee comes first
        assert!(ref2 < ref1);
    }

    #[test]
    fn test_complete_ordering() {
        // Create commitments with distinct IDs for easier verification
        let stake_high = create_test_commitment("stake_high", CommitmentType::Stake, 150);
        let stake_low = create_test_commitment("stake_low", CommitmentType::Stake, 50);
        let pledge_2_high = create_test_commitment(
            "pledge_2_high",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            200,
        );
        let pledge_2_low = create_test_commitment(
            "pledge_2_low",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            50,
        );
        let pledge_5 = create_test_commitment(
            "pledge_5",
            CommitmentType::Pledge {
                pledge_count_before_executing: 5,
            },
            100,
        );
        let pledge_10 = create_test_commitment(
            "pledge_10",
            CommitmentType::Pledge {
                pledge_count_before_executing: 10,
            },
            300,
        );
        let unstake = create_test_commitment("unstake", CommitmentType::Unstake, 75);

        let mut commitments = [
            PrioritizedCommitment(&pledge_5),
            PrioritizedCommitment(&stake_low),
            PrioritizedCommitment(&pledge_2_high),
            PrioritizedCommitment(&stake_high),
            PrioritizedCommitment(&pledge_2_low),
            PrioritizedCommitment(&pledge_10),
            PrioritizedCommitment(&unstake),
        ];

        commitments.sort();

        // Verify the expected order by checking commitment references
        // Expected order:
        // 1. stake_high (Stake, fee=150)
        // 2. stake_low (Stake, fee=50)
        // 3. pledge_2_high (Pledge count=2, fee=200)
        // 4. pledge_2_low (Pledge count=2, fee=50)
        // 5. pledge_5 (Pledge count=5, fee=100)
        // 6. pledge_10 (Pledge count=10, fee=300)
        // 7. unstake (Other type, fee=75)

        assert_eq!(commitments[0].0.borrow().id, stake_high.id);
        assert_eq!(commitments[1].0.borrow().id, stake_low.id);
        assert_eq!(commitments[2].0.borrow().id, pledge_2_high.id);
        assert_eq!(commitments[3].0.borrow().id, pledge_2_low.id);
        assert_eq!(commitments[4].0.borrow().id, pledge_5.id);
        assert_eq!(commitments[5].0.borrow().id, pledge_10.id);
        assert_eq!(commitments[6].0.borrow().id, unstake.id);
    }

    #[test]
    fn test_equality_based_on_id() {
        let commitment1 = create_test_commitment("same", CommitmentType::Stake, 100);
        let mut commitment2 = create_test_commitment("same", CommitmentType::Stake, 200);
        commitment2.id = commitment1.id; // Make IDs the same

        let ref1 = PrioritizedCommitment(&commitment1);
        let ref2 = PrioritizedCommitment(&commitment2);

        assert_eq!(ref1, ref2);
    }
}
