use actix::prelude::*;
use irys_database::insert_peer_list_item;
use irys_types::{Address, DatabaseProvider, PeerListItem};
use reth_db::Database;
use tracing::error;

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

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct AddPeerMessage(pub PeerListItem);

impl Handler<AddPeerMessage> for PeerListService {
    type Result = ();
    fn handle(&mut self, msg: AddPeerMessage, _ctx: &mut Self::Context) -> Self::Result {
        //TODO: What is the purpose of this mining address when adding a peer?
        let address = Address::random();
        let db = self.db.as_mut().expect("expected valid db");
        if let Err(e) = db
            .update(|tx| insert_peer_list_item(tx, &address, &msg.0))
            .expect("")
        {
            error!("Writing peer to db failed: {e}");
        }

        ()
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
