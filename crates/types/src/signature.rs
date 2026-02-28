use crate::{Arbitrary, IrysAddress, Signature};
use alloy_primitives::{bytes, U256 as RethU256};
use base58::{FromBase58 as _, ToBase58 as _};
use bytes::Buf as _;
use reth_codecs::Compact;
use reth_primitives_traits::crypto::secp256k1::recover_signer;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

//==============================================================================
// IrysSignature
//------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Arbitrary, Hash)]
/// Wrapper newtype around [`Signature`], with enforced boolean parity
pub struct IrysSignature(Signature);

// TODO: eventually implement ERC-2098 to save a byte

/// As base58 is rather expensive, we want to make sure that input is well formed as soon as possible
/// this constant is computed using the below `compute_max_str_len` function (to_base58() is not a const fn)
const MAX_BS58_SIG_STRING_LENGTH: usize = 89;

// #[test]
// fn compute_max_str_len() {
//     dbg!(
//         IrysSignature::new(Signature::from_bytes_and_parity(&[u8::MAX; 64], true))
//             .to_base58()
//             .len()
//     );
// }

impl IrysSignature {
    pub fn new(signature: Signature) -> Self {
        Self(signature)
    }

    /// Passthrough to the inner signature.as_bytes()
    pub fn as_bytes(&self) -> [u8; 65] {
        self.0.as_bytes()
    }

    /// Return the inner reth_signature
    pub fn reth_signature(&self) -> Signature {
        self.0
    }

    /// Validates this signature by performing signer recovery
    /// NOTE: This will silently short circuit to `false` if any part of the recovery operation errors
    pub fn validate_signature(&self, prehash: [u8; 32], expected_address: IrysAddress) -> bool {
        self.recover_signer(prehash)
            .is_ok_and(|recovered_address| expected_address == recovered_address)
    }

    pub fn recover_signer(&self, prehash: [u8; 32]) -> eyre::Result<IrysAddress> {
        Ok(recover_signer(&self.0, prehash.into())?.into())
    }

    pub fn from_base58(str: &str) -> eyre::Result<Self> {
        if str.len() > MAX_BS58_SIG_STRING_LENGTH {
            eyre::bail!(
                "Invalid signature length, max is {}, got {}",
                MAX_BS58_SIG_STRING_LENGTH,
                &str.len()
            )
        }
        // Decode the base58 string into bytes
        let decoded = str
            .from_base58()
            .map_err(|e| eyre::eyre!("Invalid base58 string: {:?}", &e))?;
        // from_raw does the 65 len check
        let sig = Signature::from_raw(&decoded)?;
        Ok(Self(sig))
    }

    pub fn to_base58(&self) -> String {
        self.0.as_bytes().to_base58()
    }
}

impl Default for IrysSignature {
    fn default() -> Self {
        Self::new(Signature::new(RethU256::ZERO, RethU256::ZERO, false))
    }
}

impl std::fmt::Debug for IrysSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("IrysSignature")
            .field(&self.to_base58())
            .finish()
    }
}

impl From<Signature> for IrysSignature {
    fn from(signature: Signature) -> Self {
        Self::new(signature)
    }
}

impl From<IrysSignature> for Signature {
    fn from(val: IrysSignature) -> Self {
        val.0
    }
}

impl<'a> From<&'a IrysSignature> for &'a Signature {
    fn from(signature: &'a IrysSignature) -> &'a Signature {
        &signature.0
    }
}

impl Compact for IrysSignature {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // the normal to/from compact impl does some whacky bitflag encoding
        // TODO: adapt it to work here
        // be careful of how the bitflags are scoped..
        // self.reth_signature.to_compact(buf)
        buf.put_slice(&self.0.r().as_le_bytes());
        buf.put_slice(&self.0.s().as_le_bytes());
        buf.put_u8(self.0.v() as u8);
        65
    }

    #[inline]
    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        use alloy_primitives::ruint::aliases::U256;
        let r = U256::from_le_slice(&buf[0..32]);
        let s = U256::from_le_slice(&buf[32..64]);
        let signature = Signature::new(r, s, buf[64] == 1);
        buf.advance(65);
        (Self::new(signature), buf)
    }
}

// Implement base58 serialization for IrysSignature
impl Serialize for IrysSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.0.as_bytes();
        serializer.serialize_str(bytes.to_base58().as_ref())
    }
}

// Implement Deserialize for IrysSignature
impl<'de> Deserialize<'de> for IrysSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // First, deserialize the base58-encoded string
        let s: String = Deserialize::deserialize(deserializer)?;

        Self::from_base58(&s).map_err(de::Error::custom)
    }
}

impl alloy_rlp::Encodable for IrysSignature {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let sig = self.0;
        alloy_rlp::Header {
            list: true,
            payload_length: sig.rlp_rs_len() + sig.v().length(),
        }
        .encode(out); // V R S encode ordering is specified by ethereum's tx signature specs
        sig.write_rlp_vrs(out, sig.v());
    }

    fn length(&self) -> usize {
        let sig = self.0;
        let payload_length = sig.rlp_rs_len() + sig.v().length();
        payload_length + alloy_rlp::length_of_length(payload_length)
    }
}

impl alloy_rlp::Decodable for IrysSignature {
    fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        let header = alloy_rlp::Header::decode(buf)?;
        let pre_len = buf.len();
        let decoded = Signature::decode_rlp_vrs(buf, bool::decode)?;
        let consumed = pre_len - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::Custom(
                "consumed incorrect number of bytes",
            ));
        }

        Ok(Self(decoded))
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{
        BoundedFee, CommitmentTransaction, CommitmentTypeV2, DataLedger, IrysAddress, Signable as _,
    };

    use crate::{irys::IrysSigner, ConsensusConfig, DataTransaction, DataTransactionHeader, H256};
    use alloy_core::hex;

    use alloy_rlp::{Decodable as _, Encodable as _};
    use k256::ecdsa::SigningKey;

    // spellchecker:off

    const DEV_PRIVATE_KEY: &str =
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

    // spellchecker:on

    #[test]
    fn data_tx_signature_signing_serialization() -> eyre::Result<()> {
        // spellchecker:off
        // from the JS Client - `txSigningParity`
        const SIG_HEX: &str = "0x2b80b5cb509d4a1b7cad4f68c44cc13b2e985c7101fe5a38668bcfeb1e79f01351e2c570ba367698228b52785b0375e3579d7a9ceef995116a25c565efa820281c";
        // Base58 encoding of the signature (for version=1 signing preimage)
        const SIG_BS58: &str = "4qfCDRG4yFjuFebpicpPk4baWjw7gtHWoBb9S3HRzLY942sQmwv216dGWPXABWN9s2n8hy1XiLNu1VmarHLDUe8VH";
        // spellchecker:on

        let testing_config = ConsensusConfig::testing();
        let irys_signer = IrysSigner {
            signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
                .unwrap(),
            chain_id: testing_config.chain_id,
            chunk_size: testing_config.chunk_size,
        };

        let original_header =
            DataTransactionHeader::V1(crate::DataTransactionHeaderV1WithMetadata {
                tx: crate::DataTransactionHeaderV1 {
                    id: Default::default(),
                    anchor: H256::from([1_u8; 32]),
                    signer: IrysAddress::ZERO,
                    data_root: H256::from([3_u8; 32]),
                    data_size: 242,
                    header_size: 0,
                    term_fee: BoundedFee::from(99_u64),
                    perm_fee: Some(BoundedFee::from(98_u64)),
                    ledger_id: DataLedger::Publish.into(),
                    bundle_format: None,
                    chain_id: testing_config.chain_id,
                    signature: Default::default(),
                },
                metadata: crate::DataTransactionMetadata::new(),
            });

        let transaction = DataTransaction {
            header: original_header,
            ..Default::default()
        };
        let transaction = irys_signer.sign_transaction(transaction)?;
        assert!(transaction.header.signature.validate_signature(
            transaction.signature_hash(),
            IrysAddress::from_slice(hex::decode(DEV_ADDRESS)?.as_slice())
        ));

        // encode and decode the signature
        //compact
        let mut bytes = Vec::new();
        transaction.header.signature.to_compact(&mut bytes);

        let (signature2, _) = IrysSignature::from_compact(&bytes, bytes.len());

        assert_eq!(transaction.header.signature, signature2);

        // serde-json base58 roundtrip
        let ser = serde_json::to_string(&transaction.header.signature)?;
        let de_ser: IrysSignature = serde_json::from_str(&ser)?;
        assert_eq!(transaction.header.signature, de_ser);

        // Verify base58 encoding matches expected (parity check with JS client)
        assert_eq!(ser, format!("\"{}\"", SIG_BS58));

        // assert parity against regenerated hex
        let decoded_js_sig = Signature::try_from(&hex::decode(&SIG_HEX[2..])?[..])?;

        let d2 = hex::encode(transaction.header.signature.as_bytes());
        // println!("{}", &d2);
        // assert_eq!(
        //     transaction.header.signature,
        //     Signature::try_from(&hex::decode(&d2)?[..])?.into()
        // );
        assert_eq!(SIG_HEX, format!("0x{}", d2));

        assert_eq!(transaction.header.signature, decoded_js_sig.into());

        // test RLP roundtrip
        let mut bytes = Vec::new();
        transaction.header.signature.encode(&mut bytes);
        assert_eq!(
            IrysSignature::decode(&mut &bytes[..]).unwrap(),
            transaction.header.signature
        );

        Ok(())
    }

    #[test]
    fn commitment_tx_signature_signing_serialization() -> eyre::Result<()> {
        // spellchecker:off
        // from the JS Client - `txSigningParity`
        const SIG_HEX: &str = "0x70b9868fde4a5e6a461a63683dd94da9dc443140fd451063088a17c22c8def1e3d351697a13aa18501cb5887b98524a1b3d060d2af8a1b877cb532f7f52a70f21b";
        // Base58 encoding of the signature (for version=1 signing preimage)
        const SIG_BS58: &str = "AwxMVRKDEnExepqnuCuFBXkiEWkg59whzZGnrGTCsYo2TQhVUUWe4ufescKiLxGVTdndCjVixGAXUvVAjEhid3nEe";
        // spellchecker:on
        let testing_config = ConsensusConfig::testing();
        let irys_signer = IrysSigner {
            signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
                .unwrap(),
            chain_id: testing_config.chain_id,
            chunk_size: testing_config.chunk_size,
        };

        let mut transaction = CommitmentTransaction::V2(crate::CommitmentV2WithMetadata {
            tx: crate::CommitmentTransactionV2 {
                id: Default::default(),
                // anchor: H256::from([1_u8; 32]),
                anchor: H256::from_base58("GqrCZEc5WU4gXj9qveAUDkNRPhsPPjWrD8buKAc5sXdZ"),

                signer: IrysAddress::ZERO,
                commitment_type: CommitmentTypeV2::Unpledge {
                    pledge_count_before_executing: u64::MAX,
                    // partition_hash: [2_u8; 32].into(),
                    partition_hash: H256::from_base58(
                        "12Yjd3YA9xjzkqDfdcXVWgyu6TpAq9WJdh6NJRWzZBKt",
                    ),
                },
                chain_id: 1270,
                signature: Default::default(),
                fee: 1234,
                value: 222.into(),
            },
            metadata: Default::default(),
        });

        irys_signer.sign_commitment(&mut transaction)?;
        assert!(transaction.signature().validate_signature(
            transaction.signature_hash(),
            IrysAddress::from_slice(hex::decode(DEV_ADDRESS)?.as_slice())
        ));

        // encode and decode the signature
        //compact
        let mut bytes = Vec::new();
        transaction.signature().to_compact(&mut bytes);

        let (signature2, _) = IrysSignature::from_compact(&bytes, bytes.len());

        assert_eq!(*transaction.signature(), signature2);

        let mut buf = Vec::new();
        transaction.encode(&mut buf);
        // bytes taken from the tx parity tests of the JS client
        assert_eq!(
            buf,
            vec![
                248, 107, 2, 160, 235, 98, 207, 255, 20, 72, 55, 95, 138, 3, 64, 73, 43, 2, 195,
                255, 161, 69, 141, 190, 217, 103, 225, 140, 51, 128, 116, 217, 123, 237, 192, 176,
                148, 100, 241, 162, 130, 158, 14, 105, 140, 24, 231, 121, 45, 110, 116, 246, 125,
                137, 170, 10, 50, 235, 3, 136, 255, 255, 255, 255, 255, 255, 255, 255, 160, 0, 101,
                118, 169, 82, 205, 224, 235, 245, 44, 177, 1, 87, 152, 203, 219, 186, 168, 206,
                177, 83, 183, 19, 181, 32, 176, 137, 138, 119, 71, 87, 223, 130, 4, 246, 130, 4,
                210, 129, 222
            ]
        );

        // serde-json base58 roundtrip
        let ser = serde_json::to_string(&transaction.signature())?;
        let de_ser: IrysSignature = serde_json::from_str(&ser)?;
        assert_eq!(*transaction.signature(), de_ser);

        // Verify base58 encoding matches expected (parity check with JS client)
        assert_eq!(ser, format!("\"{}\"", SIG_BS58));
        // dbg!(&ser);
        // println!("{}", serde_json::to_string_pretty(&transaction).unwrap());

        // assert parity against regenerated hex
        let decoded_js_sig = Signature::try_from(&hex::decode(&SIG_HEX[2..])?[..])?;

        let d2 = hex::encode(transaction.signature().as_bytes());
        // println!("{}", &d2);
        assert_eq!(
            *transaction.signature(),
            Signature::try_from(&hex::decode(&d2)?[..])?.into()
        );
        assert_eq!(SIG_HEX, format!("0x{}", d2));

        assert_eq!(*transaction.signature(), decoded_js_sig.into());

        // test RLP roundtrip
        let mut bytes = Vec::new();
        transaction.signature().encode(&mut bytes);
        assert_eq!(
            IrysSignature::decode(&mut &bytes[..]).unwrap(),
            *transaction.signature()
        );

        Ok(())
    }
}
