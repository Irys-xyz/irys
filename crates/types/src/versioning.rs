//! Generic versioning primitives for consensus data structures.
use alloy_primitives::keccak256;
use bytes::BufMut;
use reth_codecs::Compact;
use thiserror::Error;

/// Error type for versioning / decoding issues.
#[derive(Debug, Error)]
pub enum VersioningError {
    #[error("unsupported version {0}")]
    UnsupportedVersion(u8),
    #[error("version mismatch: discriminant {discriminant} != inner {inner}")]
    VersionMismatch { discriminant: u8, inner: u8 },
}

/// Marker for root objects that have a semantic version fixed by an enum discriminant.
pub trait VersionDiscriminant {
    /// Returns the u8 discriminant (stable across encoding formats and hashing preimage).
    fn version(&self) -> u8;
}

/// Implemented by inner versions (e.g., V1 struct) to declare their static version constant.
pub trait Versioned {
    const VERSION: u8;
}

/// Marker for embedded objects that implicitly inherit the parent's version.
pub trait SubVersioned {
    const PARENT_VERSION_DEPENDENT: bool = true;
}

/// Trait for types that produce a signing / hashing preimage.
pub trait Signable {
    /// Writes the canonical preimage into the provided buffer.
    fn encode_for_signing(&self, out: &mut dyn BufMut);

    /// Returns the Keccak-256 hash of the signing preimage.
    fn signature_hash(&self) -> [u8; 32] {
        let mut buf = Vec::new();
        self.encode_for_signing(&mut buf);
        keccak256(&buf).0
    }
}

/// Helper extension for inner structs that carry a `version: u8` field.
pub trait HasInnerVersion {
    fn inner_version(&self) -> u8;
}

/// Wrap a versioned inner value, validating its embedded version matches the enum discriminant constant.
pub fn assert_version<V: Versioned + HasInnerVersion>(value: &V) {
    debug_assert_eq!(
        V::VERSION,
        value.inner_version(),
        "inner object version {} does not match declared VERSION {}",
        value.inner_version(),
        V::VERSION
    );
}

/// Helper to write the discriminant + delegate compact encoding for a versioned enum.
pub fn compact_with_discriminant<E, B>(disc: u8, inner: &E, buf: &mut B) -> usize
where
    E: Compact,
    B: BufMut + AsMut<[u8]>,
{
    buf.put_u8(disc);
    1 + inner.to_compact(buf)
}

/// Helper to decode a discriminant (first byte) and return the remainder.
pub fn split_discriminant(buf: &[u8]) -> (u8, &[u8]) {
    let (first, rest) = buf.split_first().expect("buffer must contain discriminant");
    (*first, rest)
}
