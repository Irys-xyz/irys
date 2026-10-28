//! Chunk announcement and fetch tests (design revision 3, Verification).

use super::super::util::{
    FakeGossipServer, GossipServiceTestFixture, ScriptedIngest, peer_list_item, poll_until,
    wait_until_listening,
};
use crate::types::{GossipResponse, RejectionReason};
use crate::wire_types::ChunkAnnouncement;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
    DataTransactionMetadata, GossipCacheKey, GossipRequestV2, H256, PeerListItem, SendTraced as _,
    TxChunkOffset, UnpackedChunk,
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

/// The first chunk of a two-chunk root of `chunk_size`-byte chunks. Not
/// rightmost, so its path proves it at any `data_size` past its range.
fn first_of_two_chunks(chunk_size: u64, fill: u8) -> UnpackedChunk {
    let len = usize::try_from(chunk_size).expect("chunk size fits usize");
    let first = vec![fill; len];
    // clone: generate_leaves consumes the bytes the chunk keeps
    let leaves = irys_types::generate_leaves(
        [Ok(first.clone()), Ok(vec![fill.wrapping_add(1); len])].into_iter(),
        len,
    )
    .expect("leaves");
    let root = irys_types::generate_data_root(leaves).expect("root");
    let data_root = H256(root.id);
    let proof = irys_types::resolve_proofs(root, None)
        .expect("proofs")
        .into_iter()
        .next()
        .expect("first proof");
    UnpackedChunk {
        data_root,
        data_size: chunk_size * 2,
        data_path: proof.proof.into(),
        bytes: first.into(),
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
            Duration::from_secs(5),
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
/// The claim lifetime allows the client's 5s HTTP timeout and source rotation.
fn fast_limits() -> crate::chunk_announce::ChunkAnnounceLimits {
    crate::chunk_announce::ChunkAnnounceLimits {
        claim_ttl: Duration::from_secs(15),
        local_retry_delay: Duration::from_millis(200),
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

/// A served size that the body's own path cannot prove is bogus data.
#[tokio::test]
async fn body_that_does_not_prove_its_data_size_is_rejected_before_ingest() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 23);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    let fake = FakeGossipServer::new();
    let served = chunk.clone();
    fake.set_on_chunk_fetch(move |_| {
        let mut other = served.clone();
        // 32 bytes cannot be the whole of a 16-byte root.
        other.data_size = 16;
        GossipResponse::Accepted(Some(other))
    });
    let source = add_fake_peer(&receiver, 0x63, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");
    // PeerScore has no ordering; the numeric score is what "scored down" means.
    let score_before = receiver
        .peer_list
        .peer_by_id(&source.peer_id)
        .expect("source")
        .reputation_score
        .get();

    assert!(matches!(
        post_announce(
            receiver.gossip_addr(),
            &source,
            ChunkAnnouncement::from(&chunk)
        )
        .await,
        GossipResponse::Accepted(())
    ));
    poll_until(Duration::from_secs(10), "claim exhausted", || {
        handler.chunk_announce.claim_count() == 0
    })
    .await?;
    assert!(
        receiver.mempool_chunks.read().expect("lock").is_empty(),
        "nothing reached ingest"
    );
    assert!(!held(&receiver, &chunk.chunk_path_hash()));
    let score_after = receiver
        .peer_list
        .peer_by_id(&source.peer_id)
        .expect("source")
        .reputation_score
        .get();
    assert!(
        score_after < score_before,
        "a body that does not prove its data_size scores the source down"
    );
    handle.stop().await?;
    Ok(())
}

/// A later header can raise an unconfirmed root size after the announcement
/// leaves, and the source then serves the larger size. A body that still
/// proves itself is stale metadata, not bogus data: the source keeps its
/// score and the body goes to ingest, which judges the size.
#[tokio::test]
async fn body_proved_at_an_inflated_data_size_does_not_score_the_source() -> eyre::Result<()> {
    let chunk = first_of_two_chunks(32, 25);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    let fake = FakeGossipServer::new();
    let served = chunk.clone();
    fake.set_on_chunk_fetch(move |_| {
        let mut inflated = served.clone();
        inflated.data_size = 96;
        GossipResponse::Accepted(Some(inflated))
    });
    let source = add_fake_peer(&receiver, 0x65, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");
    let score_before = receiver
        .peer_list
        .peer_by_id(&source.peer_id)
        .expect("source")
        .reputation_score
        .get();

    assert!(matches!(
        post_announce(
            receiver.gossip_addr(),
            &source,
            ChunkAnnouncement::from(&chunk)
        )
        .await,
        GossipResponse::Accepted(())
    ));
    poll_until(Duration::from_secs(10), "claim released", || {
        handler.chunk_announce.claim_count() == 0
    })
    .await?;
    assert!(
        receiver
            .mempool_chunks
            .read()
            .expect("lock")
            .iter()
            .any(|ingested| ingested.data_size == 96),
        "the body reached ingest"
    );
    let score_after = receiver
        .peer_list
        .peer_by_id(&source.peer_id)
        .expect("source")
        .reputation_score
        .get();
    assert!(
        score_after >= score_before,
        "an inflated size is not bogus data: {score_before} -> {score_after}"
    );
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
async fn claim_expiry_during_ingest_frees_capacity() -> eyre::Result<()> {
    let first = valid_chunk(32, 28);
    let second = valid_chunk(32, 29);
    let mut receiver =
        GossipServiceTestFixture::new_with(|nc| nc.p2p_gossip.chunk_fetch_delay_ms = 0);
    receiver.chunk_announce_limits = Some(crate::chunk_announce::ChunkAnnounceLimits {
        claim_ttl: Duration::from_secs(2),
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

/// Every round falls inside the 30s open cooldown. The announcement proves
/// the peer reached us, so the fetch is a recovery probe and the only
/// announcer is still asked.
#[tokio::test]
async fn open_breaker_does_not_stop_the_only_announcer_being_asked() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 34);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    // clone: the fake keeps the body and the claim is built from the original
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x6d, spawn_fake(fake).await);
    let handler = receiver.data_handler().expect("handler");
    let breaker = handler.gossip_client.circuit_breaker();
    for _ in 0..5 {
        breaker.record_failure(&source.peer_id);
    }
    assert!(
        !breaker.would_allow(&source.peer_id),
        "the open cooldown has not elapsed"
    );

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
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        breaker.metrics().open_count,
        0,
        "a served body closes the breaker"
    );
    handle.stop().await?;
    Ok(())
}

/// `admit` reserves the claim before the fetch task runs. A task dropped
/// before its first poll, as at runtime shutdown, must still release it.
#[tokio::test]
async fn fetch_task_dropped_before_its_first_poll_releases_the_claim() -> eyre::Result<()> {
    use crate::chunk_announce::AnnounceDecision;

    let chunk = valid_chunk(32, 26);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    let handler = receiver.data_handler().expect("handler");
    let claim = ChunkAnnouncement::from(&chunk);
    let announcer = irys_types::IrysPeerId::from([0x66_u8; 20]);
    assert_eq!(
        handler.chunk_announce.admit(&claim, announcer),
        AnnounceDecision::StartFetch
    );

    drop(handler.chunk_fetch_task(claim));

    assert_eq!(handler.chunk_announce.claim_count(), 0);
    assert_eq!(
        handler.chunk_announce.admit(&claim, announcer),
        AnnounceDecision::StartFetch,
        "a released claim is admitted afresh"
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

#[tokio::test]
async fn local_overload_recovers_without_another_announcement() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 35);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    receiver.script_chunk_ingest(
        (0..16)
            .map(|_| {
                ScriptedIngest::Reply(Err(irys_actors::ChunkIngressError::Advisory(
                    irys_actors::AdvisoryChunkIngressError::Overloaded,
                )))
            })
            .collect(),
    );
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x74, spawn_fake(fake).await);
    let handler = receiver
        .data_handler()
        .ok_or_else(|| eyre::eyre!("handler missing"))?;

    assert!(matches!(
        post_announce(
            receiver.gossip_addr(),
            &source,
            ChunkAnnouncement::from(&chunk)
        )
        .await,
        GossipResponse::Accepted(())
    ));
    poll_until(
        Duration::from_secs(5),
        "body held after sustained local overload",
        || held(&receiver, &chunk.chunk_path_hash()),
    )
    .await?;
    assert_eq!(handler.chunk_announce.stats().announcements_received, 1);
    assert_eq!(handler.chunk_announce.stats().bodies_fetch, 1);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        5,
        "reuse each body for the bounded ingress retries"
    );
    assert_eq!(handler.chunk_announce.claim_count(), 0);
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn queue_wait_does_not_consume_fetch_budget() -> eyre::Result<()> {
    let chunk = valid_chunk(32, 36);
    let mut receiver =
        GossipServiceTestFixture::new_with(|nc| nc.p2p_gossip.chunk_fetch_delay_ms = 0);
    receiver.chunk_announce_limits = Some(crate::chunk_announce::ChunkAnnounceLimits {
        fetch_timeout: Duration::from_millis(200),
        max_concurrent_fetches: 1,
        ..fast_limits()
    });
    cache_root(&receiver, &chunk);
    let (handle, _) = receiver.run_service();
    let handler = receiver
        .data_handler()
        .ok_or_else(|| eyre::eyre!("handler missing"))?;
    let occupied = Arc::clone(&handler.chunk_announce.fetch_permits)
        .acquire_owned()
        .await?;
    let (fake, requests) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x75, spawn_fake(fake).await);
    wait_until_listening(receiver.gossip_addr()).await;

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&chunk),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        handler.chunk_announce.claim_count(),
        1,
        "queueing must not expire the network budget"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    drop(occupied);
    poll_until(Duration::from_secs(5), "queued claim delivered", || {
        held(&receiver, &chunk.chunk_path_hash())
    })
    .await?;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    handle.stop().await?;
    Ok(())
}

#[rstest::rstest]
#[case::retry(false)]
#[case::late_commit(true)]
#[tokio::test]
async fn ingest_timeout_releases_capacity_while_retaining_recovery(
    #[case] late_commit: bool,
) -> eyre::Result<()> {
    let first = valid_chunk(32, 37);
    let second = valid_chunk(32, 38);
    let mut receiver =
        GossipServiceTestFixture::new_with(|nc| nc.p2p_gossip.chunk_fetch_delay_ms = 0);
    receiver.chunk_announce_limits = Some(crate::chunk_announce::ChunkAnnounceLimits {
        max_concurrent_fetches: 1,
        ingest_timeout: Duration::from_millis(100),
        local_retry_delay: Duration::from_secs(5),
        ..fast_limits()
    });
    cache_root(&receiver, &first);
    cache_root(&receiver, &second);
    receiver.script_chunk_ingest(vec![ScriptedIngest::Stall]);
    let (handle, _) = receiver.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    let handler = receiver
        .data_handler()
        .ok_or_else(|| eyre::eyre!("handler missing"))?;
    let (fake, requests) = serving_fake(vec![first.clone(), second.clone()]);
    let source = add_fake_peer(&receiver, 0x76, spawn_fake(fake).await);
    let claim = ChunkAnnouncement::from(&first);
    post_announce(receiver.gossip_addr(), &source, claim).await;
    poll_until(Duration::from_secs(3), "first ingress submitted", || {
        !receiver
            .mempool_chunks
            .read()
            .expect("chunk store")
            .is_empty()
    })
    .await?;
    poll_until(
        Duration::from_secs(3),
        "active slot released during cooldown",
        || handler.chunk_announce.fetch_permits.available_permits() == 1,
    )
    .await?;
    assert_eq!(handler.chunk_announce.claim_count(), 1);
    assert_eq!(handler.chunk_announce.sources(&claim), vec![source.peer_id]);
    assert!(matches!(
        post_announce(receiver.gossip_addr(), &source, claim).await,
        GossipResponse::Accepted(())
    ));
    assert_eq!(
        handler.chunk_announce.claim_count(),
        1,
        "duplicate joins recovery"
    );

    post_announce(
        receiver.gossip_addr(),
        &source,
        ChunkAnnouncement::from(&second),
    )
    .await;
    poll_until(
        Duration::from_secs(3),
        "another claim progresses during cooldown",
        || held(&receiver, &second.chunk_path_hash()),
    )
    .await?;
    assert!(!held(&receiver, &first.chunk_path_hash()));
    if late_commit {
        // The original timed-out ingress can still commit while the claim is cooling down.
        cache_body(&receiver, &first);
    }
    poll_until(
        Duration::from_secs(10),
        "recovery completed and claims released",
        || held(&receiver, &first.chunk_path_hash()) && handler.chunk_announce.claim_count() == 0,
    )
    .await?;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        if late_commit { 2 } else { 3 }
    );
    assert_eq!(handler.chunk_announce.fetch_permits.available_permits(), 1);
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn cancelling_active_recovery_returns_claim_and_peer_capacity() -> eyre::Result<()> {
    use crate::chunk_announce::AnnounceDecision;

    let chunk = valid_chunk(32, 39);
    let mut receiver =
        GossipServiceTestFixture::new_with(|nc| nc.p2p_gossip.chunk_fetch_delay_ms = 0);
    receiver.chunk_announce_limits = Some(crate::chunk_announce::ChunkAnnounceLimits {
        max_claims: 1,
        max_per_peer: 1,
        max_concurrent_fetches: 1,
        ingest_timeout: Duration::from_secs(30),
        ..fast_limits()
    });
    cache_root(&receiver, &chunk);
    receiver.script_chunk_ingest(vec![ScriptedIngest::Stall]);
    let (handle, _) = receiver.run_service();
    let handler = receiver
        .data_handler()
        .ok_or_else(|| eyre::eyre!("handler missing"))?;
    let (fake, _) = serving_fake(vec![chunk.clone()]);
    let source = add_fake_peer(&receiver, 0x77, spawn_fake(fake).await);
    let claim = ChunkAnnouncement::from(&chunk);
    assert_eq!(
        handler.chunk_announce.admit(&claim, source.peer_id),
        AnnounceDecision::StartFetch
    );
    let task = tokio::spawn(handler.chunk_fetch_task(claim));
    poll_until(Duration::from_secs(3), "ingress is pending", || {
        !receiver
            .mempool_chunks
            .read()
            .expect("chunk store")
            .is_empty()
    })
    .await?;
    assert_eq!(handler.chunk_announce.fetch_permits.available_permits(), 0);

    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert_eq!(handler.chunk_announce.claim_count(), 0);
    assert_eq!(handler.chunk_announce.fetch_permits.available_permits(), 1);
    assert_eq!(
        handler.chunk_announce.admit(&claim, source.peer_id),
        AnnounceDecision::StartFetch
    );
    handler.chunk_announce.finish(&claim);
    handle.stop().await?;
    Ok(())
}

#[tokio::test]
async fn unpolled_claim_and_duplicate_keep_the_original_expiry() -> eyre::Result<()> {
    use crate::chunk_announce::AnnounceDecision;

    let chunk = valid_chunk(32, 40);
    let (receiver, handle) = receiver_for(std::slice::from_ref(&chunk), 0).await;
    let handler = receiver
        .data_handler()
        .ok_or_else(|| eyre::eyre!("handler missing"))?;
    let claim = ChunkAnnouncement::from(&chunk);
    let peer = receiver.config.peer_id();
    tokio::time::pause();
    assert_eq!(
        handler.chunk_announce.admit(&claim, peer),
        AnnounceDecision::StartFetch
    );
    let task = handler.chunk_fetch_task(claim);
    tokio::time::advance(Duration::from_secs(14)).await;
    assert_eq!(
        handler.chunk_announce.admit(&claim, peer),
        AnnounceDecision::Joined
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    let expired_at = tokio::time::Instant::now();

    task.await;

    assert_eq!(
        tokio::time::Instant::now(),
        expired_at,
        "an expired claim gets no new budget on first poll"
    );
    assert_eq!(handler.chunk_announce.claim_count(), 0);
    assert!(
        receiver
            .mempool_chunks
            .read()
            .expect("chunk store")
            .is_empty()
    );
    tokio::time::resume();
    handle.stop().await?;
    Ok(())
}

/// A second body pushed while the storage write is held must be accepted,
/// and that sender must not retry. The old path answered `Overloaded`, which
/// gossip turns into `RateLimited`, and the sender tried again.
// Current-thread is the `tokio::test` default. The occupy pause blocks the
// worker that is inside ingress, so the test needs other workers. With two,
// one worker drives all three services and, in a parallel run, the second
// push misses its 2s deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_storage_write_accepts_the_second_push_once() -> eyre::Result<()> {
    use irys_domain::{StorageModule, StorageModuleInfo, StorageModulesReadGuard};
    use irys_types::{
        DataLedger, DataTransactionHeaderV1, LedgerChunkOffset, LedgerChunkRange,
        PartitionChunkOffset, ii, ledger_chunk_offset_ii, partition::PartitionAssignment,
        partition_chunk_offset_ii,
    };
    use std::sync::RwLock;

    let modules = Arc::new(RwLock::new(Vec::new()));
    let mut receiver = GossipServiceTestFixture::new_with_storage_modules(
        |_| {},
        StorageModulesReadGuard::new(Arc::clone(&modules)),
    );
    let mut sender_a = GossipServiceTestFixture::new();
    let mut sender_b = GossipServiceTestFixture::new();
    let chunk = valid_chunk(chunk_size(&receiver), 7);
    let last_offset = receiver
        .config
        .consensus
        .num_chunks_in_partition
        .saturating_sub(1);
    let storage_module = Arc::new(StorageModule::new(
        &StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(0),
                miner_address: receiver.config.node_config.miner_address(),
                partition_hash: H256::random(),
            }),
            submodules: vec![(
                partition_chunk_offset_ii!(0_u64, last_offset),
                "hdd0".into(),
            )],
        },
        &receiver.config,
    )?);
    storage_module.pack_with_zeros();
    storage_module.force_sync_pending_chunks()?;
    let data_tx = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            data_root: chunk.data_root,
            data_size: chunk.data_size,
            ..Default::default()
        },
        metadata: DataTransactionMetadata::new(),
    });
    let tx_path = vec![5_u8, 6, 7, 8];
    storage_module.index_transaction_data(
        &data_tx,
        &tx_path,
        LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
        0,
    )?;
    modules
        .write()
        .expect("storage module list")
        .push(Arc::clone(&storage_module));
    cache_root(&receiver, &chunk);

    // The first push blocks inside occupy, on a runtime worker. Drop releases
    // it on every exit, including a failed assertion.
    storage_module.hold_ingress_after_occupy_for_test();
    struct ReleaseHold(Arc<StorageModule>);
    impl Drop for ReleaseHold {
        fn drop(&mut self) {
            self.0.release_ingress_occupy_hold_for_test();
        }
    }
    let hold = ReleaseHold(Arc::clone(&storage_module));

    receiver.add_peer(&sender_a);
    receiver.add_peer(&sender_b);
    sender_a.add_peer(&receiver);
    sender_b.add_peer(&receiver);

    let (receiver_handle, _) = receiver.run_service();
    let (sender_a_handle, sender_a_bus) = sender_a.run_service();
    let (sender_b_handle, sender_b_bus) = sender_b.run_service();
    wait_until_listening(receiver.gossip_addr()).await;
    wait_until_listening(sender_a.gossip_addr()).await;
    wait_until_listening(sender_b.gossip_addr()).await;

    let handler = receiver.data_handler().expect("handler");
    let seen_key = GossipCacheKey::Chunk(chunk.chunk_path_hash());
    let sender_b_id = sender_b.config.peer_id();

    // clone: sender A keeps its own broadcast body
    sender_a_bus
        .send_traced(GossipBroadcastMessageV2::from(chunk.clone()))
        .expect("send the first push");
    assert!(
        storage_module.ingress_occupy_hold_arrived_for_test(Duration::from_secs(2)),
        "the first push did not stop inside the storage write"
    );
    assert_eq!(
        handler.chunk_announce.stats().bodies_push,
        1,
        "the held write is one body, not a retry"
    );

    sender_b_bus
        .send_traced(GossipBroadcastMessageV2::from(chunk.clone()))
        .expect("send the second push");
    poll_until(
        Duration::from_secs(2),
        "the second push was not accepted",
        || {
            handler.chunk_announce.stats().bodies_push == 2
                && handler
                    .cache
                    .peers_that_have_seen(&seen_key)
                    .expect("seen cache")
                    .contains(&sender_b_id)
        },
    )
    .await?;
    // The first rate-limit retry waits 250ms. A second body inside this
    // window would be that retry.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        handler.chunk_announce.stats().bodies_push,
        2,
        "an accepted in-flight duplicate must not be pushed again"
    );

    drop(hold);
    poll_until(
        Duration::from_secs(5),
        "the paused write did not finish",
        || !storage_module.has_in_flight_index_for(&chunk),
    )
    .await?;
    sender_a_handle.stop().await?;
    sender_b_handle.stop().await?;
    receiver_handle.stop().await?;
    if let Some(ingress) = receiver.chunk_ingress.take() {
        ingress.stop_and_join().await;
    }
    Ok(())
}

#[tokio::test]
async fn announce_only_reaches_a_new_peer_as_an_announcement() -> eyre::Result<()> {
    use irys_types::gossip::v2::ChunkBroadcastMode;

    let mut relay = GossipServiceTestFixture::new();
    let mut receiver = GossipServiceTestFixture::new();
    relay.add_peer(&receiver);
    receiver.add_peer(&relay);
    let chunk = valid_chunk(chunk_size(&relay), 12);
    cache_body(&relay, &chunk);
    cache_body(&receiver, &chunk); // held: the announcement is accepted, no fetch
    let (h1, bus) = relay.run_service();
    let (h2, _) = receiver.run_service();
    wait_until_listening(relay.gossip_addr()).await;
    wait_until_listening(receiver.gossip_addr()).await;
    let handler = receiver.data_handler().expect("handler");

    bus.send_traced(
        GossipBroadcastMessageV2::from(chunk).with_chunk_mode(ChunkBroadcastMode::AnnounceOnly),
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

/// An old peer answers the announcement with 404. `AnnounceOnly` remembers
/// that and pushes nothing, where `Announce` would push the body.
#[tokio::test]
async fn announce_only_sends_no_body_to_an_old_peer() -> eyre::Result<()> {
    use irys_types::gossip::v2::ChunkBroadcastMode;

    let mut relay = GossipServiceTestFixture::new();
    let chunk = valid_chunk(chunk_size(&relay), 13);
    let pushes = Arc::new(AtomicUsize::new(0));
    let fake = FakeGossipServer::new();
    fake.record_chunk_pushes(Arc::clone(&pushes));
    let fake_addr = fake.spawn();
    wait_until_listening(fake_addr).await;
    let old_peer = add_fake_peer(&relay, 0x32, fake_addr);
    let (handle, bus) = relay.run_service();
    wait_until_listening(relay.gossip_addr()).await;
    // The broadcaster and the data handler share one client, so the
    // handler's copy sees the 404 the broadcaster records.
    let client = relay.data_handler().expect("handler").gossip_client.clone();

    bus.send_traced(
        GossipBroadcastMessageV2::from(chunk).with_chunk_mode(ChunkBroadcastMode::AnnounceOnly),
    )
    .expect("send");
    poll_until(
        Duration::from_secs(10),
        "the old peer's 404 was remembered",
        || client.peer_lacks_announce(&old_peer.peer_id),
    )
    .await?;
    // A fallback push would follow the 404 on the same task within
    // milliseconds; give one that must not exist time to land.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        pushes.load(Ordering::SeqCst),
        0,
        "AnnounceOnly never pushes a body"
    );
    handle.stop().await?;
    Ok(())
}
