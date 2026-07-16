use crate::utils::{IrysNodeTest, TimeMode};
use irys_types::NodeConfig;
use tracing::info;

/// In Accelerated mode, producing several blocks must not take ~1s/block, and
/// block timestamps must be strictly increasing.
#[test_log::test(tokio::test)]
async fn accelerated_time_produces_blocks_faster_than_realtime() {
    let config = NodeConfig::testing();
    let node = IrysNodeTest::new_genesis(config)
        .with_time_mode(TimeMode::Accelerated)
        .start()
        .await;

    let n = 10_u64;
    let start = std::time::Instant::now();
    node.mine_blocks(n as usize)
        .await
        .expect("mining should succeed");
    let elapsed = start.elapsed();
    let rate = n as f64 / elapsed.as_secs_f64();
    info!(
        "⏱ accelerated: mined {n} blocks in {elapsed:?} = {rate:.1} blocks/sec (real-mode floor ~1/sec)"
    );

    // Real mode would need >= ~n seconds (1 block/sec). Accelerated must be well
    // under that. Generous bound to avoid CI flake while still proving the point.
    assert!(
        elapsed.as_secs() < n,
        "expected accelerated mining < {n}s, took {elapsed:?}"
    );
    assert!(node.get_canonical_chain_height().await >= n);

    // Timestamps strictly increase across the produced chain (.timestamp via Deref).
    let mut prev = node
        .get_block_by_height(0)
        .await
        .expect("genesis")
        .timestamp;
    for h in 1..=n {
        let ts = node.get_block_by_height(h).await.expect("block").timestamp;
        assert!(
            ts.as_millis() > prev.as_millis(),
            "block {h} timestamp not strictly increasing"
        );
        prev = ts;
    }

    node.stop().await;
}
