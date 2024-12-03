use std::{
    collections::VecDeque,
    fs,
    io::{Seek, Write},
    sync::{Arc, RwLock},
};

use actix::{Actor, Context, Handler, Message};
use irys_packing::capacity_pack_range_with_data;
use irys_primitives::IrysTxId;
use irys_storage::StorageModule;
use irys_types::{
    irys::IrysSigner, Address, ChunkState, Interval, IntervalState, CHUNK_SIZE, U256,
};
use tokio::runtime::Handle;

#[derive(Debug, Message, Clone)]
#[rtype("()")]
struct PackingRequestRange {
    pub partition_id: u64,
    pub chunk_interval: Interval<u32>,
}

type AtomicChunkRange = Arc<RwLock<VecDeque<PackingRequestRange>>>;

#[derive(Debug)]
pub struct PackingActor {
    storage_module: Arc<RwLock<StorageModule>>,
    runtime_handle: Handle,
    chunks: AtomicChunkRange,
}

const CHUNK_POLL_TIME_MS: u64 = 1_000;

impl PackingActor {
    pub fn new(handle: Handle, storage_module: Arc<RwLock<StorageModule>>) -> Self {
        Self {
            runtime_handle: handle,
            storage_module,
            chunks: Arc::new(RwLock::new(VecDeque::with_capacity(1000))),
        }
    }

    async fn poll_chunks(chunks: AtomicChunkRange, storage_module: Arc<RwLock<StorageModule>>) {
        let chunk_size = storage_module.read().unwrap().config.chunk_size;

        // Loop which runs all jobs every 1 second (defined in CHUNK_POLL_TIME_MS)
        loop {
            if let Some(next_range) = chunks.read().unwrap().front() {
                let PartitionInfo {
                    filename,
                    mining_addr,
                } = get_partition_info(next_range.partition_id);

                let f = fs::File::open(filename.clone()).unwrap();

                let mut data_in_range = match fs::read(filename) {
                    Ok(r) => cast_vec_u8_to_vec_u8_array(r),
                    Err(_) => continue,
                };

                // Packs data_in_range
                capacity_pack_range_with_data(
                    &mut data_in_range,
                    mining_addr,
                    next_range.chunk_interval.start() as u64,
                    IrysTxId::random(),
                    None,
                    chunk_size as usize,
                );

                // TODO: Write to disk correctly

                // Remove from queue once complete
                let _ = chunks.write().unwrap().pop_front();
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(CHUNK_POLL_TIME_MS)).await;
        }
    }
}

fn cast_vec_u8_to_vec_u8_array<const N: usize>(input: Vec<u8>) -> Vec<[u8; N]> {
    assert!(input.len() % N == 0, "wrong input N {}", N);
    let length = input.len() / N;
    let ptr = input.as_ptr() as *const [u8; N];
    std::mem::forget(input); // So input never drops

    // safety: we've asserted that `input` length is divisible by N
    unsafe { Vec::from_raw_parts(ptr as *mut [u8; N], length, length) }
}

struct PartitionInfo {
    mining_addr: Address,
    filename: String,
}

fn get_partition_info(id: u64) -> PartitionInfo {
    PartitionInfo {
        mining_addr: Address::random(),
        filename: "".to_string(),
    }
}

impl Actor for PackingActor {
    type Context = Context<Self>;

    fn start(self) -> actix::Addr<Self> {
        // Create packing worker that runs every
        self.runtime_handle.spawn(Self::poll_chunks(
            self.chunks.clone(),
            self.storage_module.clone(),
        ));

        Context::new().run(self)
    }

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(1000000);
    }
}

impl Handler<PackingRequestRange> for PackingActor {
    type Result = ();

    fn handle(&mut self, msg: PackingRequestRange, ctx: &mut Self::Context) -> Self::Result {
        self.chunks.write().unwrap().push_back(msg);
    }
}

#[test]
fn test_casting() {
    let v: Vec<u8> = (1..=9).collect();
    let c2 = cast_vec_u8_to_vec_u8_array::<3>(v);

    assert_eq!(c2, vec![[1, 2, 3], [4, 5, 6], [7, 8, 9]])
}

#[test]
#[should_panic(expected = "wrong input N 3")]
fn test_casting_error() {
    let v: Vec<u8> = (1..=10).collect();
    let c2 = cast_vec_u8_to_vec_u8_array::<3>(v);
}
