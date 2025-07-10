use crate::{BlockHash, IrysBlockHeader};

/// A trait that is used to provide access to blocks by their hash. Used to avoid circular dependencies,
/// such as between VDF and BlockIndexService.
pub trait BlockProvider {
    fn does_block_exist(&self, hash: &BlockHash, height: u64) -> bool;
}
