//! Supply endpoint integration tests

use crate::utils::IrysNodeTest;
use irys_api_server::routes::supply::SupplyResponse;
use irys_chain::IrysNodeCtx;
use irys_types::U256;
use std::time::Duration;
use tokio::time::sleep;

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
    let ctx = IrysNodeTest::default_async().start().await;
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
    sleep(Duration::from_secs(3)).await;
    Ok(())
}

async fn fetch_supply(client: &reqwest::Client, url: &str) -> eyre::Result<SupplyResponse> {
    let response = client.get(url).send().await?;
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

/// Tests the supply endpoint with default (estimated) calculation
#[test_log::test(tokio::test)]
async fn test_supply_endpoint_estimated() -> eyre::Result<()> {
    let (ctx, client, address) = setup_test_node().await;
    ctx.mine_blocks(25).await?;

    let supply = fetch_supply(&client, &format!("{}/v1/supply", address)).await?;

    validate_supply_invariants(&ctx, &supply)?;

    ctx.stop().await;
    Ok(())
}

/// Tests the supply endpoint with exact calculation
#[test_log::test(tokio::test)]
async fn test_supply_endpoint_exact() -> eyre::Result<()> {
    let (ctx, client, address) = setup_test_node().await;
    setup_with_migrated_blocks(&ctx, 25).await?;

    let supply = fetch_supply(&client, &format!("{}/v1/supply?exact=true", address)).await?;

    assert_eq!(
        supply.calculation_method, "actual",
        "Should use actual calculation method"
    );
    assert!(
        supply.block_height > 0,
        "Block height should be positive after mining"
    );

    let amounts = parse_supply_amounts(&supply)?;

    assert_eq!(
        amounts.total,
        amounts.genesis + amounts.emitted,
        "Total supply should equal genesis + emitted"
    );

    ctx.stop().await;
    Ok(())
}

/// Tests supply endpoint handles invalid query parameters gracefully
#[test_log::test(tokio::test)]
async fn test_supply_endpoint_invalid_params() -> eyre::Result<()> {
    let (ctx, client, address) = setup_test_node().await;
    setup_with_migrated_blocks(&ctx, 10).await?;

    let response = client
        .get(format!("{}/v1/supply?exact=invalid", address))
        .send()
        .await?;

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    let response_false = client
        .get(format!("{}/v1/supply?exact=false", address))
        .send()
        .await?;

    assert_eq!(response_false.status(), reqwest::StatusCode::OK);
    let supply_info = response_false.json::<SupplyResponse>().await?;
    assert_eq!(supply_info.calculation_method, "estimated");

    ctx.stop().await;
    Ok(())
}
