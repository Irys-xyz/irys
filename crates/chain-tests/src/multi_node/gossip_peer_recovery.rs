//! Two trusted miners. Node A uses the production gossip circuit breaker
//! (5 failures, 30s cooldown). A block that B mines must land on both
//! canonical chains before either chain moves `block_migration_depth`
//! blocks past the shared ancestor.

use std::net::{Ipv4Addr, TcpListener};
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
/// A short-circuited probe is inside the 30s cooldown. Wait at most one
/// cooldown for a dial that records a fresh failure.
const PROBE_DEADLINE: Duration = Duration::from_secs(40);
/// On a healthy pull the header is in the block tree within milliseconds.
/// Kept short so it does not consume the 30s cooldown.
const ADOPT_WAIT: Duration = Duration::from_secs(3);

fn adjusted(base_secs: u64) -> Duration {
    Duration::from_secs(coverage_adjusted_timeout(base_secs))
}

fn reserve_port() -> eyre::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .wrap_err("setup: failed to reserve a local port")?;
    let port = listener
        .local_addr()
        .wrap_err("setup: failed to read the reserved port")?
        .port();
    drop(listener);
    Ok(port)
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

async fn wait_until_peer_offline(
    node: &IrysNodeTest<IrysNodeCtx>,
    peer_id: IrysPeerId,
    timeout: Duration,
) -> eyre::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let (online, _) = peer_status(node, peer_id)?;
        if !online {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("setup: peer {peer_id} stayed online within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Dial the block-pull gossip client until `FAILURES_TO_OPEN` probes fail, or
/// a probe is short-circuited after a failure dial. Either way the cooldown,
/// if the breaker opened, started on the last failure dial. A refused
/// connection counts as a failure. `GossipDisabled` counts only where the
/// client classifies it as one; where it does not, the breaker stays closed
/// and the test checks that the block is still adopted.
async fn open_pull_breaker(
    genesis: &IrysNodeTest<IrysNodeCtx>,
    peer_id: IrysPeerId,
) -> eyre::Result<()> {
    let peer = genesis
        .node_ctx
        .peer_list
        .get_peer(&peer_id)
        .ok_or_else(|| eyre::eyre!("setup: peer {peer_id} is absent from the peer list"))?;
    let client = &genesis.node_ctx.gossip_data_handler.gossip_client;
    let peer_list = &genesis.node_ctx.peer_list;
    let deadline = Instant::now() + PROBE_DEADLINE;
    let mut failure_dials = 0_u32;
    loop {
        let result = client
            .check_health(&peer_id, peer.address, peer.protocol_version, peer_list)
            .await;
        if probe_recorded_failure(&result) {
            failure_dials = failure_dials.saturating_add(1);
            if failure_dials >= FAILURES_TO_OPEN {
                info!(
                    failure_dials,
                    "pull breaker probes reached the failure threshold"
                );
                return Ok(());
            }
        } else if probe_is_short_circuit(&result) && failure_dials > 0 {
            // Closed breaker: the dial that crossed FAILURES_TO_OPEN just
            // ran. Half-open breaker: one failed recovery dial re-opens it.
            // Either way the cooldown started on that dial.
            info!(
                failure_dials,
                threshold = FAILURES_TO_OPEN,
                "pull breaker is open after a gossip health probe"
            );
            return Ok(());
        } else if !probe_is_short_circuit(&result) {
            bail!(
                "setup: gossip probe did not record a failure ({})",
                probe_text(&result)
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "setup: pull breaker did not open after {failure_dials} failure dials ({})",
                probe_text(&result)
            );
        }
        if probe_is_short_circuit(&result) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn probe_text(result: &Result<impl std::fmt::Debug, impl ToString>) -> String {
    match result {
        Ok(health) => format!("{health:?}"),
        Err(err) => err.to_string(),
    }
}

fn probe_recorded_failure(result: &Result<impl std::fmt::Debug, impl ToString>) -> bool {
    match result {
        Ok(health) => format!("{health:?}") == "Unhealthy",
        Err(err) => {
            let text = err.to_string();
            text.starts_with("Get request")
                || text.starts_with("Health check")
                || text.starts_with("Failed to get json")
        }
    }
}

fn probe_is_short_circuit(result: &Result<impl std::fmt::Debug, impl ToString>) -> bool {
    match result {
        Ok(_) => false,
        Err(err) => err.to_string().starts_with("Circuit breaker open"),
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
    peer_id: IrysPeerId,
    ancestor_height: u64,
    migration_depth: u64,
    require_offline: bool,
) -> eyre::Result<()> {
    if require_offline {
        let (online, score) = peer_status(genesis, peer_id)?;
        if online {
            bail!(
                "setup: A marked B online before B mined; the breaker cooldown elapsed (score {score})"
            );
        }
    }
    let peer_block = peer
        .mine_block()
        .await
        .wrap_err("setup: peer failed to mine the next block")?;
    let adopt_deadline = Instant::now() + ADOPT_WAIT;
    while Instant::now() < adopt_deadline {
        if genesis.get_block_by_hash(&peer_block.block_hash).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
    // Hydrate still cannot run faster than its 10s floor. This only makes
    // the periodic check ask sooner.
    genesis_config.sync.periodic_sync_check_interval_secs = 5;
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
    let http_port = reserve_port()?;
    let gossip_port = reserve_port()?;
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
    // Health marks B offline. The pull breaker is a different client, so open
    // it with dials immediately before the restart. Startup sync then has to
    // finish inside that cooldown.
    wait_until_peer_offline(&genesis, peer_id, adjusted(25)).await?;
    open_pull_breaker(&genesis, peer_id).await?;

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
    let ancestor_height = tip(&genesis)?.0;
    mine_and_expect_convergence(
        &genesis,
        &peer,
        peer_id,
        ancestor_height,
        migration_depth,
        true,
    )
    .await?;

    peer.stop().await;
    genesis.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn slow_heavy_gossip_disabled_peer_block_converges_before_migration() -> eyre::Result<()> {
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

    // Probes hit GossipDisabled and open the block-pull breaker. Re-enable
    // only after that, then mine inside the cooldown.
    open_pull_breaker(&genesis, peer_id).await?;
    peer.node_ctx.sync_state.set_gossip_broadcast_enabled(true);

    let genesis_tip = tip(&genesis)?;
    let peer_tip = tip(&peer)?;
    ensure!(
        genesis_tip == peer_tip,
        "setup: tips drifted while gossip broadcast was disabled. genesis {genesis_tip:?}. peer {peer_tip:?}"
    );
    mine_and_expect_convergence(
        &genesis,
        &peer,
        peer_id,
        genesis_tip.0,
        migration_depth,
        false,
    )
    .await?;

    peer.stop().await;
    genesis.stop().await;
    Ok(())
}
