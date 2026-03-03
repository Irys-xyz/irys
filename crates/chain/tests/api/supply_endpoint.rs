//! Supply endpoint integration tests

use crate::utils::IrysNodeTest;
use irys_api_server::routes::supply::{CalculationMethod, SupplyResponse};
use irys_chain::IrysNodeCtx;
use irys_types::{NodeConfig, U256};
use std::time::Duration;
use tokio::time::sleep;

const BLOCKS_FOR_ESTIMATED_TEST: usize = 25;
const BLOCKS_FOR_MIGRATION_TEST: usize = 30;

struct ParsedSupplyAmounts {
    total: U256,
    genesis: U256,
    emitted: U256,
    cap: U256,
}

fn parse_supply_amounts(supply: &SupplyResponse) -> eyre::Result<ParsedSupplyAmounts> {
    Ok(ParsedSupplyAmounts {
        total: U256::from_str_radix(&supply.total_supply, 10)?,
        genesis: U256::from_str_radix(&supply.genesis_supply, 10)?,
        emitted: U256::from_str_radix(&supply.emitted_supply, 10)?,
        cap: U256::from_str_radix(&supply.inflation_cap, 10)?,
    })
}

async fn setup_test_node() -> (IrysNodeTest<IrysNodeCtx>, reqwest::Client, String) {
    let config = NodeConfig::testing();
    setup_test_node_with_config(config).await
}

async fn setup_test_node_with_small_tree() -> (IrysNodeTest<IrysNodeCtx>, reqwest::Client, String) {
    let config = NodeConfig::testing().with_consensus(|c| {
        c.block_tree_depth = 5;
        c.block_migration_depth = 2;
    });
    setup_test_node_with_config(config).await
}

async fn setup_test_node_with_config(
    config: NodeConfig,
) -> (IrysNodeTest<IrysNodeCtx>, reqwest::Client, String) {
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();
    (ctx, client, address)
}

async fn setup_with_migrated_blocks(
    ctx: &IrysNodeTest<IrysNodeCtx>,
    count: usize,
) -> eyre::Result<()> {
    ctx.mine_blocks(count).await?;

    let timeout = Duration::from_secs(30);
    let poll_interval = Duration::from_millis(100);
    let start = std::time::Instant::now();

    while !ctx
        .node_ctx
        .supply_state_guard
        .as_ref()
        .map(irys_domain::SupplyStateReadGuard::is_ready)
        .unwrap_or(true)
    {
        if start.elapsed() > timeout {
            eyre::bail!("Supply state not ready after {:?}", timeout);
        }
        sleep(poll_interval).await;
    }
    Ok(())
}

async fn fetch_supply(client: &reqwest::Client, url: &str) -> eyre::Result<SupplyResponse> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();

    if status != reqwest::StatusCode::OK {
        let error_text = response.text().await?;
        eyre::bail!("Supply endpoint returned {}: {}", status, error_text);
    }

    Ok(response.json().await?)
}

fn validate_supply_invariants(
    ctx: &IrysNodeTest<IrysNodeCtx>,
    supply: &SupplyResponse,
) -> eyre::Result<()> {
    let amounts = parse_supply_amounts(supply)?;

    eyre::ensure!(
        amounts.total == amounts.genesis + amounts.emitted,
        "Total supply should equal genesis + emitted"
    );

    let expected_genesis: U256 = ctx
        .node_ctx
        .config
        .consensus
        .reth
        .alloc
        .values()
        .fold(U256::zero(), |acc, account| {
            acc + U256::from_le_bytes(account.balance.to_le_bytes())
        });
    eyre::ensure!(
        amounts.genesis == expected_genesis,
        "Genesis supply should match config"
    );

    let expected_cap = ctx
        .node_ctx
        .config
        .consensus
        .block_reward_config
        .inflation_cap
        .amount;
    eyre::ensure!(
        amounts.cap == expected_cap,
        "Inflation cap should match config"
    );

    let inflation_progress: f64 = supply.inflation_progress_percent.parse()?;
    eyre::ensure!(
        (0.0..=100.0).contains(&inflation_progress),
        "Inflation progress should be between 0 and 100"
    );

    Ok(())
}

/// Tests the supply endpoint returns valid supply data (actual or estimated based on readiness)
#[test_log::test(tokio::test)]
async fn heavy_test_supply_endpoint_returns_valid_supply() -> eyre::Result<()> {
    let (ctx, client, address) = setup_test_node().await;
    ctx.mine_blocks(BLOCKS_FOR_ESTIMATED_TEST).await?;

    let supply = fetch_supply(&client, &format!("{}/v1/supply", address)).await?;

    // Should return either Actual or Estimated depending on supply state readiness
    assert!(
        supply.calculation_method == CalculationMethod::Actual
            || supply.calculation_method == CalculationMethod::Estimated,
        "Calculation method should be Actual or Estimated, got '{:?}'",
        supply.calculation_method
    );

    validate_supply_invariants(&ctx, &supply)?;

    ctx.stop().await;
    Ok(())
}

/// Verifies actual supply calculation after block migration/pruning.
#[test_log::test(tokio::test)]
async fn heavy_test_supply_endpoint_actual() -> eyre::Result<()> {
    let (ctx, client, address) = setup_test_node_with_small_tree().await;

    // Mine blocks (6x block_tree_depth) - with block_tree_depth=5, this ensures
    // blocks have been migrated to the block index and pruned from the in-memory block tree
    let num_blocks_to_mine = BLOCKS_FOR_MIGRATION_TEST;
    setup_with_migrated_blocks(&ctx, num_blocks_to_mine).await?;

    let supply = fetch_supply(&client, &format!("{}/v1/supply", address)).await?;

    assert_eq!(
        supply.calculation_method,
        CalculationMethod::Actual,
        "Should use actual calculation method when supply state is ready"
    );
    assert!(
        supply.block_height >= num_blocks_to_mine as u64,
        "Block height should be at least {} after mining, got {}",
        num_blocks_to_mine,
        supply.block_height
    );

    validate_supply_invariants(&ctx, &supply)?;

    let amounts = parse_supply_amounts(&supply)?;
    assert!(
        amounts.emitted > U256::zero(),
        "Emitted supply should be positive after mining {} blocks",
        num_blocks_to_mine
    );

    ctx.stop().await;
    Ok(())
}
