use crate::{Arbitrary, Signature};
use alloy_primitives::{bytes, Address, U256 as RethU256};
use base58::{FromBase58, ToBase58 as _};
use bytes::Buf as _;
use reth_codecs::Compact;
use reth_primitives::transaction::recover_signer;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

//==============================================================================
// IrysSignature
//------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug, Arbitrary)]
/// Wrapper newtype around [`Signature`], with enforced [`Parity::NonEip155`] parity
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
    pub fn validate_signature(&self, prehash: [u8; 32], expected_address: Address) -> bool {
        recover_signer(&self.0, prehash.into())
            .is_ok_and(|recovered_address| expected_address == recovered_address)
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
            .map_err(|e| format!("Failed to decode from base58 {:?}", e))
            .expect("base58 should prase");

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

    use crate::{
        irys::IrysSigner, ConsensusConfig, DataTransaction, DataTransactionHeader, H256, U256,
    };
    use alloy_core::hex;
    use alloy_primitives::Address;
    use alloy_rlp::{Decodable as _, Encodable as _};
    use k256::ecdsa::SigningKey;

    // spellchecker:off

    const DEV_PRIVATE_KEY: &str =
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

    // from the JS Client - `txSigningParity`
    const SIG_HEX: &str = "0xe6f3dcdb19b05c84386ef43a9bcb0f08f9e8866f8318decf5072b3bc12fb61cf3bcb46664e0fca943fb1a4ac011ca532498707b88a8561d7493f0a9d26d8306a1c";
    // BS58 (JSON, hence the escaped quotes) encoded signature
    const SIG_BS58: &str =
        "\"MP5Nbu4GjNMiqesVbjLPacoDbpo8h78u6YwMgW3SKhP62gnvxPFeHDUiagr7dfMHFUoTBE4xwJCkx6PPktqxt8qAj\"";

    // spellchecker:on

    #[test]
    fn signature_signing_serialization() -> eyre::Result<()> {
        let testing_config = ConsensusConfig::testing();
        let irys_signer = IrysSigner {
            signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
                .unwrap(),
            chain_id: testing_config.chain_id,
            chunk_size: testing_config.chunk_size,
        };

        let original_header = DataTransactionHeader {
            id: Default::default(),
            anchor: H256::from([1_u8; 32]),
            signer: Address::ZERO,
            data_root: H256::from([3_u8; 32]),
            data_size: 1024,
            term_fee: U256::from(100_u64),
            perm_fee: Some(U256::from(1_u64)),
            ledger_id: 0,
            bundle_format: Some(0),
            chain_id: testing_config.chain_id,
            version: 0,
            ingress_proofs: None,
            signature: Default::default(),
            data_start: Some(0),
        };
        let transaction = DataTransaction {
            header: original_header,
            ..Default::default()
        };
        let transaction = irys_signer.sign_transaction(transaction)?;
        assert!(transaction.header.signature.validate_signature(
            transaction.signature_hash(),
            Address::from_slice(hex::decode(DEV_ADDRESS)?.as_slice())
        ));

        // encode and decode the signature
        //compact
        let mut bytes = Vec::new();
        transaction.header.signature.to_compact(&mut bytes);

        let (signature2, _) = IrysSignature::from_compact(&bytes, bytes.len());

        assert_eq!(transaction.header.signature, signature2);

        // serde-json
        let ser = serde_json::to_string(&transaction.header.signature)?;
        let de_ser: IrysSignature = serde_json::from_str(&ser)?;
        assert_eq!(transaction.header.signature, de_ser);

        // assert parity against hardcoded signatures
        assert_eq!(SIG_BS58, ser);
        let decoded_js_sig = Signature::try_from(&hex::decode(SIG_HEX)?[..])?;

        // let d2 = hex::encode(transaction.header.signature.as_bytes());
        // assert_eq!(
        //     transaction.header.signature,
        //     Signature::try_from(&hex::decode(&d2)?[..])?.into()
        // );
        // assert_eq!(SIG_HEX, format!("0x{}", d2));

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
}
