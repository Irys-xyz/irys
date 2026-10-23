//! Chunk announcement and fetch tests (design revision 3, Verification).

use super::super::util::{
    FakeGossipServer, GossipServiceTestFixture, ScriptedIngest, peer_list_item, poll_until,
    wait_until_listening,
};
use crate::types::{GossipResponse, RejectionReason};
use crate::wire_types::ChunkAnnouncement;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::{
    DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
    DataTransactionMetadata, GossipRequestV2, H256, PeerListItem, TxChunkOffset, UnpackedChunk,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// A self-consistent one-chunk body of `chunk_size` bytes filled with `fill`.
pub(super) fn valid_chunk(chunk_size: u64, fill: u8) -> UnpackedChunk {
    let len = usize::try_from(chunk_size).expect("chunk size fits usize");
    let bytes = vec![fill; len];
    // clone: generate_leaves consumes the iterator item and the chunk keeps the body
    let leaves =
        irys_types::generate_leaves(std::iter::once(Ok(bytes.clone())), len).expect("leaves");
    let root = irys_types::generate_data_root(leaves).expect("root");
    let data_root = H256(root.id);
    let proof = irys_types::resolve_proofs(root, None)
        .expect("proofs")
        .into_iter()
        .next()
        .expect("one proof");
    UnpackedChunk {
        data_root,
        data_size: chunk_size,
        data_path: proof.proof.into(),
        bytes: bytes.into(),
        tx_offset: TxChunkOffset::from(0),
    }
}

pub(super) fn chunk_size(fixture: &GossipServiceTestFixture) -> u64 {
    fixture.config.consensus.chunk_size
}

/// Cache only the data root, as a node that has the tx header but not the body.
pub(super) fn cache_root(fixture: &GossipServiceTestFixture, chunk: &UnpackedChunk) {
    let header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            data_root: chunk.data_root,
            data_size: chunk.data_size,
            ..Default::default()
        },
        metadata: DataTransactionMetadata::new(),
    });
    fixture
        .db
        .update_eyre(|tx| irys_database::cache_data_root(tx, &header, None).map(|_| ()))
        .expect("cache data root");
}

/// Cache the root and the body, as a node that holds the chunk.
pub(super) fn cache_body(fixture: &GossipServiceTestFixture, chunk: &UnpackedChunk) {
    cache_root(fixture, chunk);
    fixture
        .db
        .update_eyre(|tx| irys_database::cache_chunk(tx, chunk).map(|_| ()))
        .expect("cache chunk");
}

pub(super) fn truncated(chunk: &UnpackedChunk) -> UnpackedChunk {
    let mut short = chunk.clone();
    let keep = short.bytes.0.len() - 1;
    short.bytes.0.truncate(keep);
    short
}

pub(super) async fn post_announce(
    to: SocketAddr,
    from: &PeerListItem,
    announcement: ChunkAnnouncement,
) -> GossipResponse<()> {
    let body = crate::wire_types::GossipRequestV2 {
        peer_id: from.peer_id,
        miner_address: from.mining_address,
        data: announcement,
    };
    reqwest::Client::new()
        .post(format!("http://{to}/gossip/v2/chunk_announce"))
        .json(&body)
        .send()
        .await
        .expect("send announcement")
        .json()
        .await
        .expect("announcement reply is JSON")
}

pub(super) async fn post_fetch(
    to: SocketAddr,
    from: &PeerListItem,
    announcement: ChunkAnnouncement,
) -> GossipResponse<Option<crate::wire_types::UnpackedChunk>> {
    let body = crate::wire_types::GossipRequestV2 {
        peer_id: from.peer_id,
        miner_address: from.mining_address,
        data: announcement,
    };
    reqwest::Client::new()
        .post(format!("http://{to}/gossip/v2/chunk_fetch"))
        .json(&body)
        .send()
        .await
        .expect("send fetch")
        .json()
        .await
        .expect("fetch reply is JSON")
}

/// A fake announcer that serves `chunks` by claim and counts requests.
pub(super) fn serving_fake(chunks: Vec<UnpackedChunk>) -> (FakeGossipServer, Arc<AtomicUsize>) {
    let fake = FakeGossipServer::new();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    fake.set_on_chunk_fetch(move |claim: ChunkAnnouncement| {
        counter.fetch_add(1, Ordering::SeqCst);
        let found = chunks.iter().find(|chunk| {
            chunk.data_root == claim.data_root
                && chunk.tx_offset == claim.tx_offset
                && chunk.chunk_path_hash() == claim.chunk_path_hash
        });
        GossipResponse::Accepted(found.cloned())
    });
    (fake, requests)
}

/// Register a fake at `addr` in `fixture`'s peer list and return its entry.
pub(super) fn add_fake_peer(
    fixture: &GossipServiceTestFixture,
    byte: u8,
    addr: SocketAddr,
) -> PeerListItem {
    let item = peer_list_item(byte, addr, true, 50);
    fixture.peer_list.add_or_update_peer(item.clone(), true);
    item
}

pub(super) fn held(fixture: &GossipServiceTestFixture, hash: &H256) -> bool {
    fixture
        .data_handler()
        .expect("handler after run_service")
        .chunk_is_held(hash)
}

#[tokio::test]
async fn valid_push_is_held_and_counted() -> eyre::Result<()> {
    let mut receiver = GossipServiceTestFixture::new();
    let chunk = valid_chunk(chunk_size(&receiver), 3);
    cache_root(&receiver, &chunk);
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    let handler = receiver.data_handler().expect("handler");

    let request = GossipRequestV2 {
        peer_id: receiver.config.peer_id(),
        miner_address: receiver.mining_address,
        data: chunk.clone(),
    };
    handler.handle_chunk(request).await?;
    assert!(handler.chunk_is_held(&chunk.chunk_path_hash()));
    assert_eq!(handler.chunk_announce.stats().bodies_push, 1);
    assert!(!handler.chunk_announce.is_arriving(&chunk.chunk_path_hash()));
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn truncated_push_is_rejected_and_not_recorded() -> eyre::Result<()> {
    let mut receiver = GossipServiceTestFixture::new();
    let chunk = valid_chunk(chunk_size(&receiver), 4);
    cache_root(&receiver, &chunk);
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    let handler = receiver.data_handler().expect("handler");
    let source = receiver.config.peer_id();

    let request = GossipRequestV2 {
        peer_id: source,
        miner_address: receiver.mining_address,
        data: truncated(&chunk),
    };
    let error = handler
        .handle_chunk(request)
        .await
        .expect_err("a short body must not be accepted");
    assert!(
        matches!(error, crate::GossipError::InvalidData(_)),
        "got {error:?}"
    );
    assert!(!handler.chunk_is_held(&chunk.chunk_path_hash()));
    assert!(
        !handler
            .cache
            .peers_that_have_seen(&irys_types::GossipCacheKey::Chunk(chunk.chunk_path_hash()))?
            .contains(&source)
    );
    assert_eq!(handler.chunk_announce.stats().bodies_push, 0);
    assert!(!handler.chunk_announce.is_arriving(&chunk.chunk_path_hash()));
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn chunk_fetch_serves_cached_body_with_local_size() -> eyre::Result<()> {
    let mut server = GossipServiceTestFixture::new();
    let requester = GossipServiceTestFixture::new();
    server.add_peer(&requester);
    let chunk = valid_chunk(chunk_size(&server), 5);
    cache_body(&server, &chunk);
    let (handle, _) = server.run_service();
    wait_until_listening(server.gossip_addr()).await;
    let from = requester.create_default_peer_entry();

    let mut lying = ChunkAnnouncement::from(&chunk);
    lying.data_size = 1;
    match post_fetch(server.gossip_addr(), &from, lying).await {
        GossipResponse::Accepted(Some(served)) => {
            assert_eq!(served.data_size, chunk.data_size);
            assert_eq!(served.bytes, chunk.bytes);
        }
        other => panic!("expected a served body, got {other:?}"),
    }

    let mut wrong_hash = ChunkAnnouncement::from(&chunk);
    wrong_hash.chunk_path_hash = H256::random();
    assert!(matches!(
        post_fetch(server.gossip_addr(), &from, wrong_hash).await,
        GossipResponse::Accepted(None)
    ));
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn chunk_fetch_omits_a_body_that_does_not_prove_the_cached_size() -> eyre::Result<()> {
    let mut server = GossipServiceTestFixture::new();
    let requester = GossipServiceTestFixture::new();
    server.add_peer(&requester);
    let chunk = valid_chunk(chunk_size(&server), 5);
    cache_root(&server, &chunk);
    server
        .db
        .update_eyre(|tx| irys_database::cache_chunk(tx, &truncated(&chunk)).map(|_| ()))
        .expect("cache truncated chunk");
    let (handle, _) = server.run_service();
    wait_until_listening(server.gossip_addr()).await;
    let from = requester.create_default_peer_entry();

    assert!(matches!(
        post_fetch(server.gossip_addr(), &from, ChunkAnnouncement::from(&chunk)).await,
        GossipResponse::Accepted(None)
    ));
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn chunk_fetch_over_budget_is_rate_limited() -> eyre::Result<()> {
    let mut server = GossipServiceTestFixture::new();
    let requester = GossipServiceTestFixture::new();
    server.add_peer(&requester);
    let chunk = valid_chunk(chunk_size(&server), 6);
    cache_body(&server, &chunk);
    let (handle, _) = server.run_service();
    wait_until_listening(server.gossip_addr()).await;
    let handler = server.data_handler().expect("handler");

    let permits = handler.chunk_announce.serve_permits.available_permits();
    let _drained = Arc::clone(&handler.chunk_announce.serve_permits)
        .acquire_many_owned(u32::try_from(permits).expect("fits"))
        .await
        .expect("drain serve permits");
    assert!(matches!(
        post_fetch(
            server.gossip_addr(),
            &requester.create_default_peer_entry(),
            ChunkAnnouncement::from(&chunk)
        )
        .await,
        GossipResponse::Rejected(RejectionReason::RateLimited)
    ));
    handle.stop().await?;
    Ok(())
}

fn test_client(fixture: &GossipServiceTestFixture) -> crate::GossipClient {
    crate::GossipClient::new(
        Duration::from_secs(5),
        fixture.mining_address,
        fixture.config.peer_id(),
        tokio::runtime::Handle::current(),
        fixture.config.node_config.max_peer_response_bytes,
    )
}

#[tokio::test]
async fn announce_to_old_peer_reports_route_missing() {
    let fake_addr = FakeGossipServer::new().spawn();
    wait_until_listening(fake_addr).await;
    let fixture = GossipServiceTestFixture::new();
    let chunk = valid_chunk(chunk_size(&fixture), 7);
    let outcome = test_client(&fixture)
        .send_chunk_announcement(&fake_addr, &ChunkAnnouncement::from(&chunk))
        .await;
    assert!(matches!(
        outcome,
        crate::gossip_client::AnnounceOutcome::RouteMissing
    ));
}

#[tokio::test]
async fn fetch_chunk_returns_the_served_body() {
    let fixture = GossipServiceTestFixture::new();
    let chunk = valid_chunk(chunk_size(&fixture), 8);
    // clone: the fake keeps the body and the assertion still reads it
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let fake_addr = fake.spawn();
    wait_until_listening(fake_addr).await;
    let peer = add_fake_peer(&fixture, 0x21, fake_addr);

    let body = test_client(&fixture)
        .fetch_chunk(
            &peer.peer_id,
            &fake_addr,
            &ChunkAnnouncement::from(&chunk),
            &fixture.peer_list,
        )
        .await
        .expect("fetch");
    assert_eq!(body.map(|served| served.bytes), Some(chunk.bytes));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn old_peer_gets_a_full_push_after_404() -> eyre::Result<()> {
    use irys_types::SendTraced as _;
    use irys_types::gossip::v2::{ChunkBroadcastMode, GossipBroadcastMessageV2};

    let mut relay = GossipServiceTestFixture::new();
    let chunk = valid_chunk(chunk_size(&relay), 9);
    let pushes = Arc::new(AtomicUsize::new(0));
    let fake = FakeGossipServer::new();
    fake.record_chunk_pushes(Arc::clone(&pushes));
    let fake_addr = fake.spawn();
    wait_until_listening(fake_addr).await;
    add_fake_peer(&relay, 0x31, fake_addr);
    let (handle, bus) = relay.run_service();
    wait_until_listening(relay.gossip_addr()).await;

    bus.send_traced(
        GossipBroadcastMessageV2::from(chunk).with_chunk_mode(ChunkBroadcastMode::Announce),
    )
    .expect("send");
    poll_until(
        Duration::from_secs(10),
        "old peer got the full body",
        || pushes.load(Ordering::SeqCst) == 1,
    )
    .await?;
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn new_peer_gets_an_announcement_not_a_body() -> eyre::Result<()> {
    use irys_types::SendTraced as _;
    use irys_types::gossip::v2::{ChunkBroadcastMode, GossipBroadcastMessageV2};

    let mut relay = GossipServiceTestFixture::new();
    let mut receiver = GossipServiceTestFixture::new();
    relay.add_peer(&receiver);
    receiver.add_peer(&relay);
    let chunk = valid_chunk(chunk_size(&relay), 10);
    cache_body(&relay, &chunk);
    cache_body(&receiver, &chunk); // held: the announcement is accepted, no fetch
    let (h1, bus) = relay.run_service();
    let (h2, _) = receiver.run_service();
    wait_until_listening(relay.gossip_addr()).await;
    wait_until_listening(receiver.gossip_addr()).await;
    let handler = receiver.data_handler().expect("handler");

    bus.send_traced(
        GossipBroadcastMessageV2::from(chunk).with_chunk_mode(ChunkBroadcastMode::Announce),
    )
    .expect("send");
    poll_until(
        Duration::from_secs(10),
        "receiver got the announcement",
        || handler.chunk_announce.stats().announcements_received == 1,
    )
    .await?;
    assert_eq!(handler.chunk_announce.stats().bodies_push, 0);
    h1.stop().await?;
    h2.stop().await?;
    Ok(())
}

#[tokio::test]
async fn announcement_for_held_chunk_is_accepted_without_a_claim() -> eyre::Result<()> {
    let mut receiver = GossipServiceTestFixture::new();
    let announcer = GossipServiceTestFixture::new();
    receiver.add_peer(&announcer);
    let chunk = valid_chunk(chunk_size(&receiver), 11);
    cache_body(&receiver, &chunk);
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    let handler = receiver.data_handler().expect("handler");

    let from = announcer.create_default_peer_entry();
    assert!(matches!(
        post_announce(
            receiver.gossip_addr(),
            &from,
            ChunkAnnouncement::from(&chunk)
        )
        .await,
        GossipResponse::Accepted(())
    ));
    assert_eq!(handler.chunk_announce.stats().announcements_received, 1);
    assert_eq!(handler.chunk_announce.claim_count(), 0);
    assert!(
        handler
            .cache
            .peers_that_have_seen(&irys_types::GossipCacheKey::Chunk(chunk.chunk_path_hash()))?
            .contains(&from.peer_id)
    );
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn announcement_over_capacity_is_rate_limited_and_not_recorded() -> eyre::Result<()> {
    let mut receiver = GossipServiceTestFixture::new();
    let announcer = GossipServiceTestFixture::new();
    receiver.add_peer(&announcer);
    receiver.chunk_announce_limits = Some(crate::chunk_announce::ChunkAnnounceLimits {
        max_claims: 0,
        ..Default::default()
    });
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    let handler = receiver.data_handler().expect("handler");

    let chunk = valid_chunk(chunk_size(&receiver), 12);
    let from = announcer.create_default_peer_entry();
    assert!(matches!(
        post_announce(
            receiver.gossip_addr(),
            &from,
            ChunkAnnouncement::from(&chunk)
        )
        .await,
        GossipResponse::Rejected(RejectionReason::RateLimited)
    ));
    assert_eq!(handler.chunk_announce.claim_count(), 0);
    assert!(
        handler
            .cache
            .peers_that_have_seen(&irys_types::GossipCacheKey::Chunk(chunk.chunk_path_hash()))?
            .is_empty()
    );
    handle.stop().await?;
    Ok(())
}

/// Short limits so the bounded rounds finish quickly in tests.
///
/// The deadline is longer than the gossip client's 5s HTTP timeout
/// (`GossipService::with_circuit_breaker`). A deadline of 5s cancels the
/// task during a slow first response, so the next source never runs.
fn fast_limits() -> crate::chunk_announce::ChunkAnnounceLimits {
    crate::chunk_announce::ChunkAnnounceLimits {
        fetch_deadline: Duration::from_secs(15),
        later_round_delays: [Duration::from_millis(200), Duration::from_millis(200)],
        ingest_timeout: Duration::from_secs(1),
        overload_backoff: [Duration::from_millis(50); 3],
        arriving_poll: Duration::from_millis(20),
        ..Default::default()
    }
}

/// A receiver with a cached data root for each chunk, short limits and the
/// given first-round delay.
async fn receiver_for(
    chunks: &[UnpackedChunk],
    delay_ms: u64,
) -> (
    GossipServiceTestFixture,
    crate::ServiceHandleWithShutdownSignal,
) {
    let mut receiver =
        GossipServiceTestFixture::new_with(|nc| nc.p2p_gossip.chunk_fetch_delay_ms = delay_ms);
    receiver.chunk_announce_limits = Some(fast_limits());
    for chunk in chunks {
        cache_root(&receiver, chunk);
    }
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    (receiver, handle)
}

async fn spawn_fake(fake: FakeGossipServer) -> SocketAddr {
    let addr = fake.spawn();
    wait_until_listening(addr).await;
    addr
}

#[tokio::test]
async fn missing_origin_push_is_recovered_by_fetch() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 21);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    // clone: the fake keeps the body and the claim is built from the original
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x51, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");

    assert!(matches!(
        post_announce(
            receiver.gossip_addr(),
            &source,
            ChunkAnnouncement::from(&chunk)
        )
        .await,
        GossipResponse::Accepted(())
    ));
    poll_until(Duration::from_secs(10), "fetched body held", || {
        held(&receiver, &chunk.chunk_path_hash())
    })
    .await?;
    assert_eq!(handler.chunk_announce.stats().bodies_fetch, 1);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    poll_until(Duration::from_secs(2), "claim released", || {
        handler.chunk_announce.claim_count() == 0
    })
    .await?;
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_first_source_falls_through_to_the_second() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 22);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 300).await;
    let mut corrupt = chunk.clone();
    corrupt.bytes = vec![0_u8; corrupt.bytes.0.len()].into();
    let (bad, _) = serving_fake(vec![corrupt]);
    // clone: the honest fake serves the body the receiver still names
    let (good, _) = serving_fake(vec![chunk.clone()]);
    let bad_peer = add_fake_peer(&receiver, 0x61, spawn_fake(bad).await);
    let good_peer = add_fake_peer(&receiver, 0x62, spawn_fake(good).await);
    // PeerScore has no ordering; the numeric score is what "scored down" means.
    let score_before = receiver
        .peer_list
        .peer_by_id(&bad_peer.peer_id)
        .expect("bad peer")
        .reputation_score
        .get();

    let claim = ChunkAnnouncement::from(&chunk);
    post_announce(receiver.gossip_addr(), &bad_peer, claim).await;
    post_announce(receiver.gossip_addr(), &good_peer, claim).await;
    poll_until(Duration::from_secs(10), "valid body held", || {
        held(&receiver, &chunk.chunk_path_hash())
    })
    .await?;
    assert!(
        !receiver
            .mempool_chunks
            .read()
            .expect("lock")
            .iter()
            .any(|c| c.bytes.0.iter().all(|b| *b == 0)),
        "the corrupt body never reaches ingest"
    );
    let score_after = receiver
        .peer_list
        .peer_by_id(&bad_peer.peer_id)
        .expect("bad peer")
        .reputation_score
        .get();
    assert!(score_after < score_before, "bogus source scored down");
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn body_with_a_different_data_size_is_rejected_before_ingest() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 23);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    let fake = FakeGossipServer::new();
    let served = chunk.clone();
    fake.set_on_chunk_fetch(move |_| {
        let mut other = served.clone();
        other.data_size += 1;
        GossipResponse::Accepted(Some(other))
    });
    let source = add_fake_peer(&receiver, 0x63, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    poll_until(Duration::from_secs(10), "claim exhausted", || {
        handler.chunk_announce.claim_count() == 0
    })
    .await?;
    assert!(
        receiver.mempool_chunks.read().expect("lock").is_empty(),
        "nothing reached ingest"
    );
    assert!(!held(&receiver, &chunk.chunk_path_hash()));
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn truncated_push_does_not_stop_a_later_fetch() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 24);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    // clone: the fake serves the full body after the truncated push
    let (fake, _) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x64, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");

    let error = handler
        .handle_chunk(GossipRequestV2 {
            peer_id: source.peer_id,
            miner_address: source.mining_address,
            data: truncated(&chunk),
        })
        .await
        .expect_err("a short body must not be accepted");
    assert!(
        matches!(error, crate::GossipError::InvalidData(_)),
        "got {error:?}"
    );
    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    poll_until(Duration::from_secs(10), "valid body fetched", || {
        held(&receiver, &chunk.chunk_path_hash())
    })
    .await?;
    assert_eq!(handler.chunk_announce.stats().bodies_fetch, 1);
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn failed_push_during_a_fetch_resumes_from_saved_sources() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 25);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    // clone: the fake serves the body after the failed push releases
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x65, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");

    let gate = Arc::new(tokio::sync::Notify::new());
    let waiting = Arc::new(AtomicBool::new(false));
    receiver.script_chunk_ingest(vec![ScriptedIngest::Release {
        gate: Arc::clone(&gate),
        waiting: Arc::clone(&waiting),
        result: Err(irys_actors::ChunkIngressError::Critical(
            irys_actors::CriticalChunkIngressError::DatabaseError,
        )),
    }]);
    let pushed = {
        let handler = Arc::clone(&handler);
        let peer_id = source.peer_id;
        let miner_address = source.mining_address;
        // clone: the task owns the body; the announcement still uses `chunk`
        let body = chunk.clone();
        tokio::spawn(async move {
            handler
                .handle_chunk(GossipRequestV2 {
                    peer_id,
                    miner_address,
                    data: body,
                })
                .await
        })
    };
    poll_until(Duration::from_secs(5), "push is inside ingress", || {
        waiting.load(Ordering::Acquire)
    })
    .await?;
    assert!(
        handler.chunk_announce.is_arriving(&chunk.chunk_path_hash()),
        "the arriving guard must cover ingress"
    );

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "no fetch while a push is arriving"
    );
    assert_eq!(
        handler.chunk_announce.claim_count(),
        1,
        "claim and sources kept"
    );

    gate.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(2), pushed)
        .await
        .expect("push finished after ingress replied")
        .expect("push task")
        .expect_err("ingress refused the push");
    assert!(
        matches!(error, crate::GossipError::Internal(_)),
        "got {error:?}"
    );
    assert!(!handler.chunk_announce.is_arriving(&chunk.chunk_path_hash()));
    poll_until(
        Duration::from_secs(10),
        "fetched after the failed push",
        || held(&receiver, &chunk.chunk_path_hash()),
    )
    .await?;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn wrong_first_claim_does_not_block_the_correct_one() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 26);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    let (liar, _) = serving_fake(vec![]);
    // clone: the honest fake serves the body the correct claim names
    let (honest, _) = serving_fake(vec![chunk.clone()]);
    let liar_peer = add_fake_peer(&receiver, 0x66, spawn_fake(liar).await);
    let honest_peer = add_fake_peer(&receiver, 0x67, spawn_fake(honest).await);

    let mut wrong = ChunkAnnouncement::from(&chunk);
    wrong.tx_offset = TxChunkOffset::from(5_i32);
    post_announce(receiver.gossip_addr(), &liar_peer, wrong).await;
    post_announce(
        receiver.gossip_addr(),
        &honest_peer,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    poll_until(Duration::from_secs(10), "correct claim fetched", || {
        held(&receiver, &chunk.chunk_path_hash())
    })
    .await?;
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn a_backlog_announcer_takes_the_slot_of_a_failed_source() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 33);
    // A 1 s first-round delay keeps all three announcements inside round 1.
    let mut receiver =
        GossipServiceTestFixture::new_with(|nc| nc.p2p_gossip.chunk_fetch_delay_ms = 1_000);
    receiver.chunk_announce_limits = Some(crate::chunk_announce::ChunkAnnounceLimits {
        max_sources: 2,
        max_backlog: 2,
        ..fast_limits()
    });
    cache_root(&receiver, &chunk);
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    let (empty_a, a_requests) = serving_fake(vec![]);
    let (empty_b, b_requests) = serving_fake(vec![]);
    // clone: the promoted announcer serves the body
    let (good, good_requests) = serving_fake(vec![chunk.clone()]);
    let a = add_fake_peer(&receiver, 0x71, spawn_fake(empty_a).await);
    let b = add_fake_peer(&receiver, 0x72, spawn_fake(empty_b).await);
    let c = add_fake_peer(&receiver, 0x73, spawn_fake(good).await);
    let handler = receiver.data_handler().expect("handler");

    let claim = ChunkAnnouncement::from(&chunk);
    for peer in [&a, &b, &c] {
        post_announce(receiver.gossip_addr(), peer, claim).await;
    }
    assert_eq!(
        handler.chunk_announce.backlog(&claim),
        vec![c.peer_id],
        "third announcer waits"
    );

    poll_until(
        Duration::from_secs(10),
        "fetched from the promoted announcer",
        || held(&receiver, &chunk.chunk_path_hash()),
    )
    .await?;
    assert_eq!(a_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        good_requests.load(Ordering::SeqCst),
        1,
        "promoted and asked in the same round"
    );
    assert_eq!(
        b_requests.load(Ordering::SeqCst),
        0,
        "found before the second source's turn"
    );
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn parked_fetch_is_fetched_again_in_a_later_round() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 27);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    receiver.script_chunk_ingest(vec![ScriptedIngest::Reply(Ok(()))]); // Ok, no row: a park
    // clone: the fake serves the body on both rounds
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x68, spawn_fake(fake).await);

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    poll_until(Duration::from_secs(10), "held after the re-fetch", || {
        held(&receiver, &chunk.chunk_path_hash())
    })
    .await?;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "one fetch per round until the row exists"
    );
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn stalled_ingest_ends_by_deadline_and_frees_capacity() -> eyre::Result<()> {
    let first = valid_chunk(32, 28);
    let second = valid_chunk(32, 29);
    let mut receiver =
        GossipServiceTestFixture::new_with(|nc| nc.p2p_gossip.chunk_fetch_delay_ms = 0);
    receiver.chunk_announce_limits = Some(crate::chunk_announce::ChunkAnnounceLimits {
        fetch_deadline: Duration::from_secs(2),
        ingest_timeout: Duration::from_secs(30), // longer than the deadline
        max_concurrent_fetches: 1,
        ..fast_limits()
    });
    cache_root(&receiver, &first);
    cache_root(&receiver, &second);
    receiver.script_chunk_ingest(vec![ScriptedIngest::Stall]);
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    let (fake, _) = serving_fake(vec![first.clone(), second.clone()]);
    let source = add_fake_peer(&receiver, 0x69, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&first),
    )
    .await;
    poll_until(
        Duration::from_secs(5),
        "stalled claim released by deadline",
        || handler.chunk_announce.claim_count() == 0,
    )
    .await?;
    assert_eq!(
        handler.chunk_announce.fetch_permits.available_permits(),
        1,
        "permit returned"
    );

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&second),
    )
    .await;
    poll_until(Duration::from_secs(5), "a later claim progresses", || {
        held(&receiver, &second.chunk_path_hash())
    })
    .await?;
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn exhausted_sources_stop_after_three_rounds() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 30);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    let (fake, requests) = serving_fake(vec![]); // always None
    let source = add_fake_peer(&receiver, 0x6a, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    poll_until(Duration::from_secs(10), "claim released", || {
        handler.chunk_announce.claim_count() == 0
    })
    .await?;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "one request per source per round"
    );
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn overloaded_ingest_is_retried_with_the_same_body() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 31);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    receiver.script_chunk_ingest(vec![ScriptedIngest::Reply(Err(
        irys_actors::ChunkIngressError::Advisory(
            irys_actors::AdvisoryChunkIngressError::Overloaded,
        ),
    ))]);
    // clone: the fake serves the body once; overload retries reuse it
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x6b, spawn_fake(fake).await);

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    poll_until(Duration::from_secs(10), "held after one overload", || {
        held(&receiver, &chunk.chunk_path_hash())
    })
    .await?;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the body was kept, not re-fetched"
    );
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn chunk_held_while_awaiting_a_fetch_permit_is_not_fetched() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 32);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    // clone: the fake would serve the body if the task still fetched it
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x6c, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");

    let permits = handler.chunk_announce.fetch_permits.available_permits();
    let drained = Arc::clone(&handler.chunk_announce.fetch_permits)
        .acquire_many_owned(u32::try_from(permits).expect("fits"))
        .await
        .expect("drain fetch permits");
    assert!(matches!(
        post_announce(
            receiver.gossip_addr(),
            &source,
            ChunkAnnouncement::from(&chunk),
        )
        .await,
        GossipResponse::Accepted(())
    ));
    poll_until(Duration::from_secs(5), "fetch claim admitted", || {
        handler.chunk_announce.claim_count() == 1
    })
    .await?;
    // The task passes its round's held check, then waits for a permit.
    tokio::time::sleep(Duration::from_millis(500)).await;
    cache_body(&receiver, &chunk);
    drop(drained);

    poll_until(Duration::from_secs(5), "claim released", || {
        handler.chunk_announce.claim_count() == 0
    })
    .await?;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "a body held before the permit is not fetched again"
    );
    assert_eq!(handler.chunk_announce.stats().bodies_fetch, 0);
    handle.stop().await?;
    Ok(())
}
