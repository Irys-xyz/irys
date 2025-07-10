use crate::{IrysBlockHeader};

/// A trait that is used to provide access to blocks by their hash. Used to avoid circular dependencies,
/// such as between VDF and BlockIndexService.
pub trait BlockProvider {
    fn latest_canonical_block(&self) -> Option<&IrysBlockHeader>;
}
