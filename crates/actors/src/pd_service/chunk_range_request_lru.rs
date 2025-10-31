#[derive(Clone)]
pub enum ChunkRangeRequestState {
    Requesting {
        expire_height: usize,
    },
    Provisioned {
        is_locked: bool,
        expire_height: usize,
    },
    Expired,
}
