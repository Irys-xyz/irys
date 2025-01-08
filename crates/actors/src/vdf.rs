use actix::prelude::*;
use nodit::{interval::ii, InclusiveInterval, Interval};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock, RwLockReadGuard},
};
use tracing::info;

use irys_types::{block_production::Seed, H256List, H256};

#[derive(Debug)]
pub struct VdfState {
    /// last global step stored
    pub global_step: u64,
    pub seeds: VecDeque<Seed>,
}

impl VdfState {
    /// Push new seed, and removing oldest one if is full
    pub fn push_step(&mut self, seed: Seed) {
        if self.seeds.len() >= self.seeds.capacity() {
            self.seeds.pop_front();
        }

        self.global_step += 1;
        self.seeds.push_back(seed);
        info!(
            "Received seed: {:?} global step: {}",
            self.seeds.back().unwrap(),
            self.global_step
        );
    }

    /// Get steps in the given global steps numbers Interval
    pub fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        let vdf_steps_len = self.seeds.len() as u64;

        let last_global_step = self.global_step;
        let first_global_step = last_global_step - vdf_steps_len + 1;

        if !ii(first_global_step, last_global_step).contains_interval(&i) {
            return Err(eyre::eyre!(
                "Unavailable requested range ({}..={}). Stored steps range is ({}..={})",
                i.start(),
                i.end(),
                first_global_step,
                last_global_step
            ));
        }

        let start: usize = (i.start() - first_global_step).try_into()?;
        let end: usize = (i.end() - first_global_step).try_into()?;

        Ok(H256List(
            self.seeds
                .range(start..=end)
                .map(|seed| seed.0)
                .collect::<Vec<H256>>(),
        ))
    }
}

#[derive(Debug)]
pub struct VdfService(pub Arc<RwLock<VdfState>>);

impl Default for VdfService {
    fn default() -> Self {
        Self::new(100)
    }
}

impl VdfService {
    /// Creates a new `VdfService` setting up how many steps are stored in memory
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(RwLock::new(VdfState {
            global_step: 0,
            seeds: VecDeque::with_capacity(capacity),
        })))
    }
}

impl Supervised for VdfService {}

impl ArbiterService for VdfService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {}
}

// Actor implementation
impl Actor for VdfService {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        info!("Vdf service started!");
    }
}

/// Send the most recent mining step to all the `PartitionMiningActors`
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct VdfSeed(pub Seed);

// Handler for SeedMessage
impl Handler<VdfSeed> for VdfService {
    type Result = ();

    fn handle(&mut self, msg: VdfSeed, _ctx: &mut Context<Self>) -> Self::Result {
        self.0.write().unwrap().push_step(msg.0);
    }
}

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
pub struct VdfStepsReadGuard(Arc<RwLock<VdfState>>);

impl VdfStepsReadGuard {
    /// Creates a new `ReadGard` for Ledgers
    pub const fn new(state: Arc<RwLock<VdfState>>) -> Self {
        Self(state)
    }

    /// Read access to internal steps queue
    pub fn read(&self) -> RwLockReadGuard<'_, VdfState> {
        self.0.read().unwrap()
    }
}

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "VdfStepsReadGuard")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetVdfStateMessage;

impl Handler<GetVdfStateMessage> for VdfService {
    type Result = VdfStepsReadGuard; // Return guard directly

    fn handle(&mut self, _msg: GetVdfStateMessage, _ctx: &mut Self::Context) -> Self::Result {
        VdfStepsReadGuard::new(self.0.clone())
    }
}

// Tests
#[cfg(test)]
mod tests {
    use irys_types::H256;

    use super::*;

    #[actix_rt::test]
    async fn test_vdf() {
        let addr = VdfService::new(4).start();

        // Send 8 seeds 1,2..,8 (capacity is 4)
        for i in 0..8 {
            addr.send(VdfSeed(Seed(H256([(i + 1) as u8; 32]))))
                .await
                .unwrap();
        }

        let state = addr.send(GetVdfStateMessage).await.unwrap();

        let steps = state
            .read()
            .seeds
            .iter().cloned()
            .collect::<Vec<_>>();

        // Should only contain last 3 messages
        assert_eq!(steps.len(), 4);

        // Check last 4 seeds are stored
        for i in 0..4 {
            assert_eq!(steps[i], Seed(H256([(i + 5) as u8; 32])));
        }

        // range not stored
        let get_error = state.read().get_steps(ii(3, 5));
        assert!(get_error.is_err());

        // ok inner range
        let get = state.read().get_steps(ii(6, 7)).unwrap();
        assert_eq!(H256List(vec![H256([6; 32]), H256([7; 32])]), get);

        // complete stored range
        let get_all = state.read().get_steps(ii(5, 8)).unwrap();
        assert_eq!(
            H256List(vec![
                H256([5; 32]),
                H256([6; 32]),
                H256([7; 32]),
                H256([8; 32])
            ]),
            get_all
        );
    }
}
