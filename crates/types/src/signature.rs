use crate::{Arbitrary, Signature};
use alloy_primitives::{bytes, ruint::aliases::U256, Address, Parity, B256, U256 as RethU256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use base58::{FromBase58, ToBase58 as _};
use bytes::Buf as _;
use eyre::eyre;
use reth_codecs::Compact;
use reth_primitives::transaction::recover_signer;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
//==============================================================================
// IrysSignature
//------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug, Arbitrary, RlpEncodable, RlpDecodable)]
/// Wrapper newtype around [`Signature`], with enforced [`Parity::NonEip155`] parity
pub struct IrysSignature(Signature);

// TODO: eventually implement ERC-2098 to save a byte

impl IrysSignature {
    pub fn new(signature: Signature) -> IrysSignature {
        IrysSignature(signature).with_eth_parity()
    }

    /// converts the parity to a bool, then to the Ethereum standard NonEip155 parity (27 or 28)
    pub fn with_eth_parity(mut self) -> Self {
        self.0 = self.0.with_parity(Parity::NonEip155(self.0.v().y_parity()));
        self
    }

    /// Passthough to the inner signature.as_bytes()
    pub fn as_bytes(&self) -> [u8; 65] {
        self.0.as_bytes()
    }

    /// Return the inner reth_signature
    pub fn reth_signature(&self) -> Signature {
        self.0
    }

    /// Validates this signature by performing signer recovery  
    /// NOTE: This will silently short circuit to `false` if any part of the recovery operation errors
    pub fn validate_signature(&self, prehash: B256, expected_address: Address) -> bool {
        recover_signer(&self.0, prehash).map_or(false, |recovered_address| {
            expected_address == recovered_address
        })
    }
}

impl Default for IrysSignature {
    fn default() -> Self {
        IrysSignature::new(Signature::new(
            RethU256::ZERO,
            RethU256::ZERO,
            Parity::Parity(false),
        ))
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
        buf.put_u8(self.0.v().y_parity_byte());
        65
    }

    #[inline]
    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        // let bitflags = buf.get_u8() as usize;
        // let sig_bit = bitflags & 1;
        // let (signature, buf2) = Signature::from_compact(buf, sig_bit);

        let r = U256::from_le_slice(&buf[0..32]);
        let s = U256::from_le_slice(&buf[32..64]);
        let signature = Signature::new(r, s, Parity::NonEip155(buf[64] == 1));
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        irys::IrysSigner, IrysTransaction, IrysTransactionHeader, H256, IRYS_CHAIN_ID,
        MAX_CHUNK_SIZE,
    };
    use alloy_core::hex::{self};
    use alloy_primitives::Address;
    use k256::ecdsa::SigningKey;

    const DEV_PRIVATE_KEY: &str =
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";
    // from the js-client, for the tx header in signer_test
    const JS_SIGNATURE: &str = "0x0515dc91d5dbb4bbe9a030e5c9039eea484332391a5441a7aadd9af31b4ce4cc1a3269151ce7a01c50e33d5e0fe2f80699525e15b9a01dc567d74dabf0c3964a1c";

    // BS58 (JSON, hence the escaped quotes) encoded signature
    const SIG_BS58: &str =
        "\"T2eUuL9SqvdCTiW1vt2mwrH2rBqaS1TKoouePL2x7wLDkA9npixpiFBY9He4ZDhkVLVR13GRd55FCojiGW4Sy7s1\"";

    #[test]
    fn signature_signing_serialization() -> eyre::Result<()> {
        let irys_signer = IrysSigner {
            signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
                .unwrap(),
            chain_id: IRYS_CHAIN_ID,
            chunk_size: MAX_CHUNK_SIZE,
        };

        let original_header = IrysTransactionHeader {
            id: Default::default(),
            anchor: H256::from([1u8; 32]),
            signer: Address::ZERO,
            data_root: H256::from([3u8; 32]),
            data_size: 1024,
            term_fee: 100,
            perm_fee: Some(1),
            ledger_num: 0,
            bundle_format: Some(0),
            chain_id: IRYS_CHAIN_ID,
            version: 0,
            signature: Default::default(),
        };
        let transaction = IrysTransaction {
            header: original_header,
            ..Default::default()
        };
        let transaction = irys_signer.sign_transaction(transaction)?;
        assert!(transaction.header.signature.validate_signature(
            transaction.signature_hash().into(),
            Address::from_slice(hex::decode(DEV_ADDRESS)?.as_slice())
        ));
        let decoded_js_sig = Signature::try_from(&hex::decode(JS_SIGNATURE)?[..])?;
        assert_eq!(transaction.header.signature, decoded_js_sig.into());

        // encode and decode the signature
        //compact
        let mut bytes = Vec::new();
        transaction.header.signature.to_compact(&mut bytes);

        let (signature2, _) = IrysSignature::from_compact(&bytes, bytes.len());

        assert_eq!(transaction.header.signature, signature2);

        // serde-json
        let ser = serde_json::to_string(&transaction.header.signature)?;
        assert_eq!(SIG_BS58, ser);
        let de_ser: IrysSignature = serde_json::from_str(&ser)?;

        assert_eq!(transaction.header.signature, de_ser);

        Ok(())
    }
}
