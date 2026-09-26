use irys_types::ingress::IngressProof;
use irys_types::{BlockHash, H256};
use std::collections::{HashMap, HashSet};

/// Cap on parked unknown-anchor proofs. Signature and stake already passed, so
/// this bounds in-flight proofs from staked miners whose anchors we have not
/// imported yet — not an untrusted flood.
pub(crate) const MAX_PENDING_INGRESS_PROOFS: usize = 512;

/// Ingress proofs parked because their anchor block is not yet known locally.
///
/// Keyed by [`IngressProof::id`] (signature hash). Indexed by anchor so a
/// newly imported block can drain only the proofs waiting on it.
#[derive(Debug)]
pub(crate) struct PendingIngressProofs {
    by_id: HashMap<H256, IngressProof>,
    by_anchor: HashMap<BlockHash, HashSet<H256>>,
    max_entries: usize,
}

impl PendingIngressProofs {
    pub(crate) fn new(max_entries: usize) -> Self {
        Self {
            by_id: HashMap::new(),
            by_anchor: HashMap::new(),
            max_entries: max_entries.max(1),
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Insert or replace a parked proof.
    ///
    /// Returns `false` if this is a new id and the cache is at capacity (the
    /// proof is not stored). Replacing an existing id always succeeds.
    pub(crate) fn put(&mut self, proof: IngressProof) -> bool {
        let id = proof.id();
        let new_anchor = proof.anchor;
        if let Some(existing) = self.by_id.get(&id) {
            if existing.anchor != new_anchor {
                self.unindex(id, existing.anchor);
                self.index(id, new_anchor);
            }
            self.by_id.insert(id, proof);
            return true;
        }
        if self.by_id.len() >= self.max_entries {
            return false;
        }
        self.index(id, new_anchor);
        self.by_id.insert(id, proof);
        true
    }

    /// Remove and return every proof parked for `anchor`.
    pub(crate) fn take_for_anchor(&mut self, anchor: BlockHash) -> Vec<IngressProof> {
        let Some(ids) = self.by_anchor.remove(&anchor) else {
            return Vec::new();
        };
        ids.into_iter()
            .filter_map(|id| self.by_id.remove(&id))
            .collect()
    }

    fn index(&mut self, id: H256, anchor: BlockHash) {
        self.by_anchor.entry(anchor).or_default().insert(id);
    }

    fn unindex(&mut self, id: H256, anchor: BlockHash) {
        if let Some(ids) = self.by_anchor.get_mut(&anchor) {
            ids.remove(&id);
            if ids.is_empty() {
                self.by_anchor.remove(&anchor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::ingress::IngressProofV1;
    use irys_types::{IrysSignature, Signature};

    fn dummy_proof(sig_seed: u64, anchor: H256) -> IngressProof {
        let mut r_s = [0_u8; 64];
        r_s[0] = sig_seed as u8;
        r_s[32] = (sig_seed >> 8) as u8;
        IngressProof::V1(IngressProofV1 {
            signature: IrysSignature::new(Signature::from_bytes_and_parity(&r_s, false)),
            data_root: H256::zero(),
            proof: H256::zero(),
            chain_id: 0,
            anchor,
        })
    }

    #[test]
    fn put_rejects_when_at_capacity() {
        let mut cache = PendingIngressProofs::new(2);
        let a = H256::from([1_u8; 32]);
        assert!(cache.put(dummy_proof(1, a)));
        assert!(cache.put(dummy_proof(2, a)));
        assert!(!cache.put(dummy_proof(3, a)));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn replacing_existing_id_does_not_consume_capacity() {
        let mut cache = PendingIngressProofs::new(1);
        let a = H256::from([1_u8; 32]);
        let b = H256::from([2_u8; 32]);
        let first = dummy_proof(1, a);
        let id = first.id();
        assert!(cache.put(first));
        let mut replacement = dummy_proof(1, b);
        // same signature → same id, different anchor
        replacement.anchor = b;
        assert_eq!(replacement.id(), id);
        assert!(cache.put(replacement));
        assert_eq!(cache.len(), 1);
        assert!(cache.take_for_anchor(a).is_empty());
        assert_eq!(cache.take_for_anchor(b).len(), 1);
    }

    #[test]
    fn take_for_anchor_leaves_other_anchors() {
        let mut cache = PendingIngressProofs::new(8);
        let a = H256::from([1_u8; 32]);
        let b = H256::from([2_u8; 32]);
        assert!(cache.put(dummy_proof(1, a)));
        assert!(cache.put(dummy_proof(2, a)));
        assert!(cache.put(dummy_proof(3, b)));

        let drained = cache.take_for_anchor(a);
        assert_eq!(drained.len(), 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.take_for_anchor(b).len(), 1);
        assert_eq!(cache.len(), 0);
    }
}
