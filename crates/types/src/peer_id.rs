use crate::{Arbitrary, IrysAddress};
use reth_codecs::Compact;
use reth_db::DatabaseError;
use reth_db_api::table::{Decode, Encode};

/// A newtype wrapper for peer network identifier.
/// Uses the same underlying type as IrysAddress (20 bytes) but represents
/// a separate identity for P2P networking, distinct from mining/staking address.
/// It can be created from an address, but it is strictly forbidden to convert it back or access
///  the inner address to avoid the temptation to use those types interchangeably.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
    Arbitrary,
)]
#[serde(transparent)]
#[repr(transparent)]
pub struct IrysPeerId(IrysAddress);

impl IrysPeerId {
    /// Zero peer ID constant
    pub const ZERO: Self = Self(IrysAddress::ZERO);

    /// Create a new random peer ID
    #[inline]
    pub fn random() -> Self {
        Self(IrysAddress::random())
    }
}

impl core::fmt::Debug for IrysPeerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "IrysPeerId({:?})", self.0)
    }
}

impl core::fmt::Display for IrysPeerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<IrysAddress> for IrysPeerId {
    #[inline]
    fn from(addr: IrysAddress) -> Self {
        Self(addr)
    }
}

impl From<[u8; 20]> for IrysPeerId {
    #[inline]
    fn from(bytes: [u8; 20]) -> Self {
        Self(IrysAddress::from(bytes))
    }
}

impl From<IrysPeerId> for [u8; 20] {
    #[inline]
    fn from(peer_id: IrysPeerId) -> Self {
        peer_id.0.into()
    }
}

impl AsRef<[u8]> for IrysPeerId {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl alloy_rlp::Encodable for IrysPeerId {
    #[inline]
    fn length(&self) -> usize {
        alloy_rlp::Encodable::length(&self.0)
    }

    #[inline]
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::Encodable::encode(&self.0, out)
    }
}

impl alloy_rlp::Decodable for IrysPeerId {
    #[inline]
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        <IrysAddress as alloy_rlp::Decodable>::decode(buf).map(Self)
    }
}

impl Compact for IrysPeerId {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.0.to_compact(buf)
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (addr, buf) = IrysAddress::from_compact(buf, len);
        (Self(addr), buf)
    }
}

impl Encode for IrysPeerId {
    type Encoded = [u8; 20];

    fn encode(self) -> Self::Encoded {
        <IrysAddress as Encode>::encode(self.0)
    }
}

impl Decode for IrysPeerId {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        <IrysAddress as Decode>::decode(value).map(Self)
    }
}
