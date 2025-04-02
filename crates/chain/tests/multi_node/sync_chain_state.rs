use irys_types::Config;

#[actix_web::test]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    let mut test_config = Config::testnet();
    test_config.port = 8080;

    //start genesis
    //start mining
    //start two additional peers, instructing them to use the genesis peer as their trusted peer

    Ok(())
}
