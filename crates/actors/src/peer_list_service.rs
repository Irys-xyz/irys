use actix::prelude::*;
use irys_types::DatabaseProvider;

#[derive(Debug, Default)]
pub struct PeerListService {
    /// Reference to the node database
    #[allow(dead_code)]
    db: Option<DatabaseProvider>,
}

impl Actor for PeerListService {
    type Context = Context<Self>;
}

/// Allows this actor to live in the the service registry
impl Supervised for PeerListService {}

impl SystemService for PeerListService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("service started: peer_list");
    }
}

impl PeerListService {
    /// Create a new instance of the peer_list_service actor passing in a reference
    /// counted reference to a `DatabaseEnv`
    pub fn new(db: DatabaseProvider) -> Self {
        println!("service started: peer_list");
        Self { db: Some(db) }
    }
}
