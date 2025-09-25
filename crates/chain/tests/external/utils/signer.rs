use eyre::Result;
use irys_types::{irys::IrysSigner, Address, ConsensusConfig};
use k256::ecdsa::SigningKey;

#[derive(Debug, Clone)]
pub(crate) struct TestSigner {
    pub irys_signer: IrysSigner,
    pub address: Address,
    pub name: String,
}

impl TestSigner {
    pub(crate) fn from_private_key(
        private_key_hex: &str,
        name: String,
        config: &ConsensusConfig,
    ) -> Result<Self> {
        // Parse hex private key
        let key_bytes = hex::decode(private_key_hex.trim_start_matches("0x"))?;

        // Create signing key
        let signing_key = SigningKey::from_slice(&key_bytes)?;

        // Create IrysSigner
        let irys_signer = IrysSigner {
            signer: signing_key,
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        let address = irys_signer.address();

        Ok(Self {
            irys_signer,
            address,
            name,
        })
    }

    pub(crate) fn random(name: String, config: &ConsensusConfig) -> Self {
        let irys_signer = IrysSigner::random_signer(config);
        let address = irys_signer.address();

        Self {
            irys_signer,
            address,
            name,
        }
    }
}

pub(crate) fn load_test_signers_from_env() -> Result<Vec<String>> {
    // Load private keys from environment variables
    // Format: TEST_SIGNER_KEYS="key1,key2,key3"
    let keys_env = std::env::var("TEST_SIGNER_KEYS");

    match keys_env {
        Ok(keys) => {
            let parsed: Vec<String> = keys
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            if parsed.is_empty() {
                Err(eyre::eyre!("No valid keys found in TEST_SIGNER_KEYS"))
            } else {
                Ok(parsed)
            }
        }
        Err(_) => {
            tracing::warn!("TEST_SIGNER_KEYS not set, using default test keys");
            // Default test keys for development (DO NOT USE IN PRODUCTION)
            // These are well-known Hardhat test keys
            Ok(vec![
                "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
                "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d".to_string(),
                "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a".to_string(),
            ])
        }
    }
}
