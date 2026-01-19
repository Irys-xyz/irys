use std::path::PathBuf;

use irys_types::{serde_utils, CommitmentTransaction, IrysTransactionCommon as _};
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};

#[test]
fn commitment_tx_signature_signing_serialization() -> eyre::Result<()> {
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TxSigningTestData {
        tx: CommitmentTransaction,
        #[serde(
            deserialize_with = "serde_utils::signing_key_from_hex",
            serialize_with = "serde_utils::serializes_signing_key"
        )]
        // currently unused
        r#priv: SigningKey,
    }

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/commitments.json");
    let test_data = std::fs::read_to_string(path)?;

    let data: Vec<TxSigningTestData> = serde_json::from_str(&test_data)?;
    for tx in data {
        assert!(
            tx.tx.is_signature_valid(),
            "signature should be valid for tx with ID {}",
            &tx.tx.id()
        )
    }

    Ok(())
}
