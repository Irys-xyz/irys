use std::sync::Arc;

use actix::{Actor, Context, Handler, Message};
use irys_storage::partition_provider::PartitionStorageProvider;
use irys_types::{ChunkBin, ChunkState, Interval, IntervalState};

#[derive(Debug, Clone)]
pub struct ChunkStorageActor {
    storage_provider: PartitionStorageProvider,
}

impl ChunkStorageActor {
    pub fn new(storage_provider: PartitionStorageProvider) -> Self {
        Self { storage_provider }
    }
}

impl Actor for ChunkStorageActor {
    type Context = Context<Self>;
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "eyre::Result<Arc<Vec<ChunkBin>>>")]
pub struct ReadChunks {
    pub interval: Interval<u32>,
    // optional state requirement for a read
    pub expected_state: Option<ChunkState>,
}

impl Handler<ReadChunks> for ChunkStorageActor {
    type Result = eyre::Result<Arc<Vec<ChunkBin>>>;

    fn handle(&mut self, read_req: ReadChunks, _ctx: &mut Context<Self>) -> Self::Result {
        Ok(Arc::new(
            self.storage_provider
                .read_chunks(read_req.interval, read_req.expected_state)?,
        ))
    }
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "eyre::Result<()>")]
pub struct WriteChunks {
    pub interval: Interval<u32>,
    // not Arc as the actor blocks while copying and then writing the chunks anyway
    pub chunks: Vec<ChunkBin>,
    pub expected_state: ChunkState,
    pub new_state: IntervalState,
}

impl Handler<WriteChunks> for ChunkStorageActor {
    type Result = eyre::Result<()>;

    fn handle(&mut self, write_req: WriteChunks, _ctx: &mut Context<Self>) -> Self::Result {
        let WriteChunks {
            interval,
            chunks,
            expected_state,
            new_state,
        } = write_req;

        self.storage_provider
            .write_chunks(chunks, interval, expected_state, new_state)
    }
}
