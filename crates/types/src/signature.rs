use crate::{Arbitrary, Signature};
use alloy_primitives::{bytes, ruint::aliases::U256, Address, U256 as RethU256};
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
    pub fn new(signature: Signature) -> IrysSignature {
        IrysSignature(signature)
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
        recover_signer(&self.0, prehash.into()).is_ok_and(|recovered_address| {
            expected_address == recovered_address
        })
    }
}

impl Default for IrysSignature {
    fn default() -> Self {
        IrysSignature::new(Signature::new(RethU256::ZERO, RethU256::ZERO, false))
    }
}

impl From<Signature> for IrysSignature {
    fn from(signature: Signature) -> Self {
        IrysSignature::new(signature)
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
        // let bitflags = buf.get_u8() as usize;
        // let sig_bit = bitflags & 1;
        // let (signature, buf2) = Signature::from_compact(buf, sig_bit);

        let r = U256::from_le_slice(&buf[0..32]);
        let s = U256::from_le_slice(&buf[32..64]);
        let signature = Signature::new(r, s, buf[64] == 1);
        buf.advance(65);
        (IrysSignature::new(signature), buf)
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
        Ok(IrysSignature::new(sig))
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

        Ok(IrysSignature(decoded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{irys::IrysSigner, ConsensusConfig, IrysTransaction, IrysTransactionHeader, H256};
    use alloy_core::hex;
    use alloy_primitives::Address;
    use alloy_rlp::{Decodable, Encodable};
    use k256::ecdsa::SigningKey;

    // spellchecker:off

    const DEV_PRIVATE_KEY: &str =
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

    // from the JS Client - `txSigningParity`
    const SIG_HEX: &str = "0x090fc65dab8667ab4d8fd0989a6a8faebd7a73d9e9d7c7b705e84904b84b839052eb0a266525e1f8cb6811669e6dc459997da4dc2d4c6002483b77442af58ea91c";
    // BS58 (JSON, hence the escaped quotes) encoded signature
    const SIG_BS58: &str =
        "\"oP2QpwKkSS6JB3mSBKQh5ULmp1fZHQdjmPApCRcPn32YFK1iMCi12TpEJnmP1njRT5SsNgEewZb3umjraoxcDTv3\"";

    // spellchecker:on

    #[test]
    fn signature_signing_serialization() -> eyre::Result<()> {
        let testnet_config = ConsensusConfig::testnet();
        let irys_signer = IrysSigner {
            signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
                .unwrap(),
            chain_id: testnet_config.chain_id,
            chunk_size: testnet_config.chunk_size,
        };

        let original_header = IrysTransactionHeader {
            id: Default::default(),
            anchor: H256::from([1u8; 32]),
            signer: Address::ZERO,
            data_root: H256::from([3u8; 32]),
            data_size: 1024,
            term_fee: 100,
            perm_fee: Some(1),
            ledger_id: 0,
            bundle_format: Some(0),
            chain_id: testnet_config.chain_id,
            version: 0,
            ingress_proofs: None,
            signature: Default::default(),
        };
        let transaction = IrysTransaction {
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
