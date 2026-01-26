use crate::{Arbitrary, IrysAddress};

/// Newtype wrapper for peer network identifier.
/// Uses the same underlying type as IrysAddress (20 bytes) but represents
/// a separate identity for P2P networking, distinct from mining/staking address.
/// This is a distinct type to enforce separation from IrysAddress at compile time.
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
pub struct IrysPeerId(pub IrysAddress);

impl IrysPeerId {
    /// Zero peer ID constant
    pub const ZERO: Self = Self(IrysAddress::ZERO);

    /// Create a new random peer ID
    #[inline]
    pub fn random() -> Self {
        Self(IrysAddress::random())
    }

    /// Get the inner address
    #[inline]
    pub const fn inner(&self) -> &IrysAddress {
        &self.0
    }

    /// Convert to the inner address
    #[inline]
    pub const fn into_inner(self) -> IrysAddress {
        self.0
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

impl From<IrysPeerId> for IrysAddress {
    #[inline]
    fn from(peer_id: IrysPeerId) -> Self {
        peer_id.0
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

impl AsRef<IrysAddress> for IrysPeerId {
    #[inline]
    fn as_ref(&self) -> &IrysAddress {
        &self.0
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
        self.0.length()
    }

    #[inline]
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.0.encode(out)
    }
}

impl alloy_rlp::Decodable for IrysPeerId {
    #[inline]
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        IrysAddress::decode(buf).map(Self)
    }
}
