use crate::{Arbitrary, IrysAddress, Signature};
use alloy_primitives::{bytes, U256 as RethU256};
use base58::{FromBase58, ToBase58 as _};
use bytes::Buf as _;
use reth_codecs::Compact;
use reth_primitives::transaction::recover_signer;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

//==============================================================================
// IrysSignature
//------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug, Arbitrary, Hash)]
/// Wrapper newtype around [`Signature`], with enforced boolean parity
pub struct IrysSignature(Signature);

// TODO: eventually implement ERC-2098 to save a byte

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
}

impl Default for IrysSignature {
    fn default() -> Self {
        Self::new(Signature::new(RethU256::ZERO, RethU256::ZERO, false))
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

        // Decode the base58 string into bytes
        let bytes = FromBase58::from_base58(s.as_str())
            .map_err(|e| de::Error::custom(format!("Failed to decode from base58 {:?}", e)))?;

        // Ensure the byte array is exactly 65 bytes (r, s, and v values of the signature)
        if bytes.len() != 65 {
            return Err(de::Error::invalid_length(
                bytes.len(),
                &"expected 65 bytes for signature",
            ));
        }

        // Convert the byte array into a Signature struct using TryFrom
        let sig = Signature::try_from(bytes.as_slice()).map_err(de::Error::custom)?;

        // Return the IrysSignature by wrapping the Signature
        Ok(Self::new(sig))
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
    use crate::{BoundedFee, CommitmentTransaction, CommitmentTypeV1, IrysAddress, Signable as _};

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

        let original_header = DataTransactionHeader::V1(crate::DataTransactionHeaderV1 {
            id: Default::default(),
            anchor: H256::from([1_u8; 32]),
            signer: IrysAddress::ZERO,
            data_root: H256::from([3_u8; 32]),
            data_size: 242,
            header_size: 0,
            term_fee: BoundedFee::from(99_u64),
            perm_fee: Some(BoundedFee::from(98_u64)),
            ledger_id: 0,
            bundle_format: None,
            chain_id: testing_config.chain_id,
            promoted_height: None,
            signature: Default::default(),
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
        const SIG_HEX: &str = "0x559103e7798f0523cf37487224ddd6c2189276be86ec85d3b2ae7468b097f616173ba6d3056266f80dbeab90d793e6734dba1d92e81427e716bcd8853cca25fe1b";
        // Base58 encoding of the signature (for version=1 signing preimage)
        const SIG_BS58: &str = "8YxBZcsHQXARCXnhcd5svVYWCYEBZWcWf8pE5TPai39ogdT3WXfWyd6VZ73duogV1t81pwTU1gdKj9nJQ9DiJAL2W";
        // spellchecker:on
        let testing_config = ConsensusConfig::testing();
        let irys_signer = IrysSigner {
            signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
                .unwrap(),
            chain_id: testing_config.chain_id,
            chunk_size: testing_config.chunk_size,
        };

        let mut transaction = CommitmentTransaction::V1(crate::CommitmentTransactionV1 {
            id: Default::default(),
            anchor: H256::from([1_u8; 32]),
            signer: IrysAddress::ZERO,
            commitment_type: CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 12,
                partition_hash: [2_u8; 32].into(),
            },
            chain_id: testing_config.chain_id,
            signature: Default::default(),
            fee: 1234,
            value: 222.into(),
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
