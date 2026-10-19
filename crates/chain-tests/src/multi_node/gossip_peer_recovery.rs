//! Two trusted miners. The genesis node uses the production gossip circuit
//! breaker (5 failures, 30s cooldown). One case mines while that breaker
//! is open for the peer. The other answers health with `GossipDisabled`
//! and must leave the breaker closed. In both, the block the peer mines
//! must land on both canonical chains before either chain moves
//! `block_migration_depth` blocks past the shared ancestor.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use eyre::{WrapErr as _, bail, ensure};
use irys_chain::IrysNodeCtx;
use irys_types::irys::IrysSigner;
use irys_types::{BlockHash, IrysPeerId, NodeConfig};
use tracing::info;

use crate::utils::{IrysNodeTest, TimeMode, coverage_adjusted_timeout};

/// Production breaker. The gossip client that pulls block bodies is not the
/// client that scores inactive peers, so a frozen score does not open it.
const FAILURES_TO_OPEN: u32 = 5;
/// Bound for the unreachable health dials that open the pull breaker.
/// A refused connection returns immediately. The bound covers a dial that
/// waits out the client timeout instead.
const PROBE_DEADLINE: Duration = Duration::from_secs(40);
/// Longer than the mine that follows the trusted-peer restart. The sync
/// check's hydration pass is the only other probe of the pull client, and
/// a successful health reply closes the breaker.
const PULL_BREAKER_QUIET_SECS: u64 = 3_600;

fn adjusted(base_secs: u64) -> Duration {
    Duration::from_secs(coverage_adjusted_timeout(base_secs))
}

/// On a healthy pull the header is in the block tree within milliseconds.
/// Kept short so the wait does not consume the 30s open cooldown. Coverage
/// builds multiply the bound the same way as the other waits.
fn adopt_wait() -> Duration {
    adjusted(3)
}

/// Listeners held until the peer node binds. The harness binds its own
/// sockets and does not take a caller listener. `SO_REUSEADDR` does not
/// allow both listeners on the same port, so these are dropped on the line
/// before `start`. Holding them until then keeps the ports reserved across
/// the genesis restart.
struct ReservedPorts {
    http: TcpListener,
    gossip: TcpListener,
}

impl ReservedPorts {
    fn bind() -> eyre::Result<Self> {
        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .wrap_err("setup: failed to reserve a local http port")?;
        let gossip = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .wrap_err("setup: failed to reserve a local gossip port")?;
        Ok(Self { http, gossip })
    }

    fn http_port(&self) -> eyre::Result<u16> {
        Ok(self
            .http
            .local_addr()
            .wrap_err("setup: failed to read the reserved http port")?
            .port())
    }

    fn gossip_port(&self) -> eyre::Result<u16> {
        Ok(self
            .gossip
            .local_addr()
            .wrap_err("setup: failed to read the reserved gossip port")?
            .port())
    }
}

fn tip(node: &IrysNodeTest<IrysNodeCtx>) -> eyre::Result<(u64, BlockHash)> {
    let chain = node.get_canonical_chain();
    let block = chain
        .last()
        .ok_or_else(|| eyre::eyre!("setup: canonical chain is empty"))?;
    Ok((block.height(), block.block_hash()))
}

fn peer_status(node: &IrysNodeTest<IrysNodeCtx>, peer_id: IrysPeerId) -> eyre::Result<(bool, u16)> {
    let peer = node
        .node_ctx
        .peer_list
        .get_peer(&peer_id)
        .ok_or_else(|| eyre::eyre!("setup: peer {peer_id} is absent from the peer list"))?;
    Ok((peer.is_online, peer.reputation_score.get()))
}

async fn wait_until_peer_online(
    node: &IrysNodeTest<IrysNodeCtx>,
    peer_id: IrysPeerId,
    timeout: Duration,
) -> eyre::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(peer) = node.node_ctx.peer_list.get_peer(&peer_id)
            && peer.is_online
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("setup: peer {peer_id} was not online within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_startup_sync(
    peer: &IrysNodeTest<IrysNodeCtx>,
    genesis: &IrysNodeTest<IrysNodeCtx>,
    timeout: Duration,
) -> eyre::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let genesis_tip = tip(genesis)?;
        let peer_tip = tip(peer)?;
        let broadcast = peer.node_ctx.sync_state.is_gossip_broadcast_enabled();
        let syncing = peer.node_ctx.sync_state.is_syncing();
        if peer_tip == genesis_tip && broadcast && !syncing {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "setup: peer did not finish startup sync. peer tip {peer_tip:?}. genesis tip {genesis_tip:?}. broadcast={broadcast}. syncing={syncing}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn pull_client_metrics(
    genesis: &IrysNodeTest<IrysNodeCtx>,
) -> irys_utils::circuit_breaker::CircuitBreakerMetrics<IrysPeerId> {
    genesis
        .node_ctx
        .gossip_data_handler
        .gossip_client
        .circuit_breaker_metrics()
}

fn pull_breaker_is_open(genesis: &IrysNodeTest<IrysNodeCtx>, peer_id: IrysPeerId) -> bool {
    pull_client_metrics(genesis).open_keys.contains(&peer_id)
}

fn breaker_summary(genesis: &IrysNodeTest<IrysNodeCtx>) -> String {
    let metrics = pull_client_metrics(genesis);
    format!(
        "open {} half-open {} keys {:?}",
        metrics.open_count, metrics.half_open_count, metrics.open_keys
    )
}

/// Open the block-pull breaker for `peer_id` by dialing health at an
/// unreachable address.
///
/// The breaker key is the peer id. `check_health` does not write the dial
/// address into the peer list, so the source-peer body pull still uses the
/// peer's real address. Localhost port 1 refuses the connection, and that
/// refusal records a failure. Call this after startup sync: a health probe
/// during that sync records success for a live peer and closes a breaker
/// that was opened while the peer was down. The sync-check interval set in
/// `start_trusted_miners` is longer than the mine that follows, so that
/// pass cannot close this breaker before the body pull. The open cooldown
/// starts on the dial that trips the threshold, which is this call.
async fn open_pull_breaker(
    genesis: &IrysNodeTest<IrysNodeCtx>,
    peer_id: IrysPeerId,
) -> eyre::Result<()> {
    let peer = genesis
        .node_ctx
        .peer_list
        .get_peer(&peer_id)
        .ok_or_else(|| eyre::eyre!("setup: peer {peer_id} is absent from the peer list"))?;
    let mut unreachable = peer.address;
    unreachable.gossip = SocketAddr::from((Ipv4Addr::LOCALHOST, 1));
    let client = &genesis.node_ctx.gossip_data_handler.gossip_client;
    let peer_list = &genesis.node_ctx.peer_list;
    let deadline = Instant::now() + PROBE_DEADLINE;
    loop {
        let result = client
            .check_health(&peer_id, unreachable, peer.protocol_version, peer_list)
            .await;
        if pull_breaker_is_open(genesis, peer_id) {
            info!("pull breaker is open for the peer");
            return Ok(());
        }
        // A reply did not record a failure. A transport error did, unless
        // the dial was skipped because the breaker was already open. That
        // case returns above.
        if result.is_ok() {
            bail!("setup: unreachable health dial returned a reply ({result:?})");
        }
        if Instant::now() >= deadline {
            bail!(
                "setup: pull breaker did not open for peer {peer_id} ({result:?}, {})",
                breaker_summary(genesis)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn assert_converged_before_migration(
    genesis: &IrysNodeTest<IrysNodeCtx>,
    peer: &IrysNodeTest<IrysNodeCtx>,
    ancestor_height: u64,
    migration_depth: u64,
    timeout: Duration,
) -> eyre::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let (genesis_height, genesis_hash) = tip(genesis)?;
        let (peer_height, peer_hash) = tip(peer)?;
        let same = genesis_height == peer_height && genesis_hash == peer_hash;
        let advanced = genesis_height
            .max(peer_height)
            .saturating_sub(ancestor_height);
        if same && genesis_height > ancestor_height && advanced < migration_depth {
            return Ok(());
        }
        if genesis_height == peer_height && genesis_hash != peer_hash {
            bail!(
                "fork: nodes diverged at height {genesis_height}. A block {genesis_hash}. B block {peer_hash}. ancestor height {ancestor_height}. migration depth {migration_depth}"
            );
        }
        if advanced >= migration_depth && !same {
            bail!(
                "fork: a node advanced {advanced} blocks past ancestor {ancestor_height} without a shared tip. A height {genesis_height} block {genesis_hash}. B height {peer_height} block {peer_hash}"
            );
        }
        if Instant::now() >= deadline {
            if !same {
                bail!(
                    "fork: nodes did not converge. A height {genesis_height} block {genesis_hash}. B height {peer_height} block {peer_hash}. ancestor height {ancestor_height}"
                );
            }
            bail!(
                "setup: nodes stayed on the ancestor tip height {genesis_height} block {genesis_hash}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn mine_and_expect_convergence(
    genesis: &IrysNodeTest<IrysNodeCtx>,
    peer: &IrysNodeTest<IrysNodeCtx>,
    ancestor_height: u64,
    migration_depth: u64,
) -> eyre::Result<()> {
    let peer_block = peer
        .mine_block()
        .await
        .wrap_err("setup: peer failed to mine the next block")?;
    let peer_hash = peer_block.block_hash;
    let adopt_for = adopt_wait();
    let adopt_deadline = Instant::now() + adopt_for;
    let mut adopted = false;
    while Instant::now() < adopt_deadline {
        if genesis.get_block_by_hash(&peer_hash).is_ok() {
            adopted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(
        adopted,
        "adoption: genesis did not adopt peer block {peer_hash} within {adopt_for:?} before genesis mined its child"
    );
    genesis
        .mine_block()
        .await
        .wrap_err("setup: genesis failed to mine the block after the peer")?;
    assert_converged_before_migration(
        genesis,
        peer,
        ancestor_height,
        migration_depth,
        adjusted(20),
    )
    .await
}

async fn assign_peer_partition(
    genesis: &IrysNodeTest<IrysNodeCtx>,
    peer: &IrysNodeTest<IrysNodeCtx>,
    peer_signer: &IrysSigner,
) -> eyre::Result<()> {
    let seconds = 20;
    let stake_tx = peer
        .post_stake_commitment(None)
        .await
        .wrap_err("setup: failed to post stake")?;
    let pledge_tx = peer
        .post_pledge_commitment(None)
        .await
        .wrap_err("setup: failed to post pledge")?;
    genesis
        .wait_for_mempool(stake_tx.id(), seconds)
        .await
        .wrap_err("setup: stake did not reach the genesis mempool")?;
    genesis
        .wait_for_mempool(pledge_tx.id(), seconds)
        .await
        .wrap_err("setup: pledge did not reach the genesis mempool")?;

    let height = genesis.get_canonical_chain_height().await;
    genesis
        .mine_block()
        .await
        .wrap_err("setup: failed to mine the commitment block")?;
    peer.wait_until_height(height + 1, seconds)
        .await
        .wrap_err("setup: peer missed the commitment block")?;

    let num_blocks_in_epoch = genesis.node_ctx.config.consensus.epoch.num_blocks_in_epoch;
    let current = genesis.get_canonical_chain_height().await;
    let blocks_until_next_epoch = num_blocks_in_epoch - (current % num_blocks_in_epoch);
    for _ in 0..blocks_until_next_epoch {
        let before = genesis.get_canonical_chain_height().await;
        genesis
            .mine_block()
            .await
            .wrap_err("setup: failed to mine toward the next epoch")?;
        peer.wait_until_height(before + 1, seconds)
            .await
            .wrap_err("setup: peer missed a block before the epoch")?;
    }

    peer.wait_for_packing(seconds).await;
    let assignments = peer.get_partition_assignments(peer_signer.address());
    ensure!(
        !assignments.is_empty(),
        "setup: peer has no partition assignment"
    );
    let genesis_tip = tip(genesis)?;
    let peer_tip = tip(peer)?;
    ensure!(
        genesis_tip == peer_tip,
        "setup: tips differ after partition assignment. genesis {genesis_tip:?}. peer {peer_tip:?}"
    );
    Ok(())
}

async fn start_trusted_miners()
-> eyre::Result<(IrysNodeTest<IrysNodeCtx>, IrysNodeTest<IrysNodeCtx>, u64)> {
    let mut genesis_config = NodeConfig::testing_with_epochs(2);
    // Test VDF steps are floored at 25ms. Free-run parks at the second reset
    // boundary until a block is migrated. The default testing reset (1200)
    // is that boundary after about 60s, and this test does not migrate, so
    // mining would stall before the breaker check. A wider reset keeps the
    // gate outside the test. The breaker cooldown stays 30s either way.
    genesis_config.consensus.get_mut().vdf.reset_frequency = 20_000;
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Real clock. The breaker uses a monotonic clock, and the harness
    // otherwise picks accelerated time at random.
    let genesis = IrysNodeTest::new_genesis(genesis_config)
        .with_time_mode(TimeMode::Real)
        .start_and_wait_for_packing("GENESIS", 20)
        .await;

    let mut peer_config = genesis.testing_peer_with_signer(&peer_signer);
    let reserved = ReservedPorts::bind()?;
    let http_port = reserved.http_port()?;
    let gossip_port = reserved.gossip_port()?;
    peer_config.http.bind_port = http_port;
    peer_config.http.public_port = http_port;
    peer_config.gossip.bind_port = gossip_port;
    peer_config.gossip.public_port = gossip_port;
    let peer_addr = peer_config.peer_address();
    let trusted_genesis = peer_config.trusted_peers[0];

    // Trust is fixed at peer-list startup. Restart genesis with the peer
    // address before the peer boots, so the peer never dials a dead genesis.
    let mut genesis = genesis.stop().await;
    genesis.cfg.trusted_peers = vec![peer_addr];
    // Hydration on the pull client records success and closes the breaker.
    // The restart test opens that breaker and then mines. This interval
    // outlasts that mine, so the pass cannot close the breaker first.
    // The peer config was copied before this assignment and keeps the
    // default interval.
    genesis.cfg.sync.periodic_sync_check_interval_secs = PULL_BREAKER_QUIET_SECS;
    let genesis = genesis.start_and_wait_for_packing("GENESIS", 20).await;
    ensure!(
        genesis.get_gossip_addr() == trusted_genesis.gossip,
        "setup: genesis gossip address changed on restart (was {}, now {})",
        trusted_genesis.gossip,
        genesis.get_gossip_addr()
    );
    ensure!(
        genesis.get_peer_addr() == trusted_genesis.api,
        "setup: genesis api address changed on restart (was {}, now {})",
        trusted_genesis.api,
        genesis.get_peer_addr()
    );

    // Drop on this line. `start` binds the same ports.
    drop(reserved);
    let peer = IrysNodeTest::new(peer_config).start_with_name("PEER").await;
    let peer_id = peer.node_ctx.config.peer_id();
    let genesis_peer_id = genesis.node_ctx.config.peer_id();
    wait_until_peer_online(&genesis, peer_id, adjusted(30)).await?;
    wait_until_peer_online(&peer, genesis_peer_id, adjusted(30)).await?;
    assign_peer_partition(&genesis, &peer, &peer_signer).await?;

    let migration_depth = u64::from(genesis.node_ctx.config.consensus.block_migration_depth);
    Ok((genesis, peer, migration_depth))
}

#[test_log::test(tokio::test)]
async fn slow_heavy_restarted_peer_block_converges_before_migration() -> eyre::Result<()> {
    let (genesis, peer, migration_depth) = start_trusted_miners().await?;
    let peer_id = peer.node_ctx.config.peer_id();
    let gossip_addr = peer.get_gossip_addr();
    let api_addr = peer.get_peer_addr();

    let peer = peer.stop().await;
    // `stop` is the proof the process is down. The online flag moves only
    // when the sync-check hydration probes this pull client. That pass stays
    // quiet for the rest of the test, so it cannot record success and close
    // the breaker opened after startup sync. The flag can stay true while
    // the process is down. It is not the signal this test waits on.

    let peer = peer.start_with_name("PEER").await;
    ensure!(
        peer.node_ctx.config.peer_id() == peer_id,
        "setup: peer id changed on restart"
    );
    ensure!(
        peer.get_gossip_addr() == gossip_addr,
        "setup: peer gossip address changed on restart (was {gossip_addr}, now {})",
        peer.get_gossip_addr()
    );
    ensure!(
        peer.get_peer_addr() == api_addr,
        "setup: peer api address changed on restart (was {api_addr}, now {})",
        peer.get_peer_addr()
    );

    wait_for_startup_sync(&peer, &genesis, adjusted(20)).await?;
    // Unreachable dials for this peer id. The peer list keeps the real
    // address, so the body pull of the block mined next still reaches the
    // peer. The breaker has to be open at that pull.
    open_pull_breaker(&genesis, peer_id).await?;
    ensure!(
        pull_breaker_is_open(&genesis, peer_id),
        "setup: pull breaker is not open for peer {peer_id} immediately before the peer mines ({})",
        breaker_summary(&genesis)
    );
    let ancestor_height = tip(&genesis)?.0;
    mine_and_expect_convergence(&genesis, &peer, ancestor_height, migration_depth).await?;

    peer.stop().await;
    genesis.stop().await;
    Ok(())
}

/// Broadcast off, reception on: health answers `GossipDisabled`, and the
/// peer still serves pulls. Those replies must not open the block-pull
/// breaker. The peer then mines, and both chains adopt the block before
/// migration.
#[test_log::test(tokio::test)]
async fn slow_heavy_gossip_disabled_peer_stays_usable_and_block_converges() -> eyre::Result<()> {
    let (genesis, peer, migration_depth) = start_trusted_miners().await?;
    let peer_id = peer.node_ctx.config.peer_id();
    ensure!(
        peer.node_ctx.sync_state.is_gossip_broadcast_enabled(),
        "setup: peer gossip broadcast is already disabled"
    );
    ensure!(
        peer.node_ctx.sync_state.is_gossip_reception_enabled(),
        "setup: peer gossip reception is disabled"
    );
    let (online, _) = peer_status(&genesis, peer_id)?;
    ensure!(online, "setup: peer is offline before gossip is disabled");

    // Same shape as startup sync: broadcast off, reception still on.
    // Health then answers GossipDisabled.
    peer.node_ctx.sync_state.set_gossip_broadcast_enabled(false);
    ensure!(
        !peer.node_ctx.sync_state.is_gossip_broadcast_enabled(),
        "setup: gossip broadcast stayed enabled"
    );
    ensure!(
        peer.node_ctx.sync_state.is_gossip_reception_enabled(),
        "setup: disabling broadcast also disabled reception"
    );

    // Enough replies to open the production breaker when GossipDisabled
    // records a failure. A transport error is a setup failure: the peer
    // has to answer.
    let peer_entry = genesis
        .node_ctx
        .peer_list
        .get_peer(&peer_id)
        .ok_or_else(|| eyre::eyre!("setup: peer {peer_id} is absent from the peer list"))?;
    let client = &genesis.node_ctx.gossip_data_handler.gossip_client;
    let peer_list = &genesis.node_ctx.peer_list;
    for _ in 0..FAILURES_TO_OPEN {
        let result = client
            .check_health(
                &peer_id,
                peer_entry.address,
                peer_entry.protocol_version,
                peer_list,
            )
            .await;
        ensure!(
            result.is_ok(),
            "setup: health probe did not get a reply while gossip broadcast is disabled ({result:?})"
        );
    }
    // Broadcast back on so the mined block is pushed. The replies above
    // are what decide the breaker.
    peer.node_ctx.sync_state.set_gossip_broadcast_enabled(true);

    let genesis_tip = tip(&genesis)?;
    let peer_tip = tip(&peer)?;
    ensure!(
        genesis_tip == peer_tip,
        "setup: tips drifted while gossip broadcast was disabled. genesis {genesis_tip:?}. peer {peer_tip:?}"
    );
    ensure!(
        !pull_breaker_is_open(&genesis, peer_id),
        "breaker: GossipDisabled health replies opened the pull breaker for peer {peer_id} ({})",
        breaker_summary(&genesis)
    );
    mine_and_expect_convergence(&genesis, &peer, genesis_tip.0, migration_depth).await?;

    peer.stop().await;
    genesis.stop().await;
    Ok(())
}
