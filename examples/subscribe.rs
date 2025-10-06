use alloy::{
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use alloy_provider::WsConnect;
use amms::{amms::uniswap_v4::UniswapV4Factory, state_space::StateSpaceBuilder};
use futures::StreamExt;
use std::sync::Arc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .ws(WsConnect::new(rpc_endpoint))
        .await?;

    let sync_provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let factories = vec![UniswapV4Factory::new(
        address!("0x000000000004444c5dc75cB358380D2e3dE08A90"),
        21688329,
    )
    .into()];

    let state_space_manager = StateSpaceBuilder::new(sync_provider.clone())
        .with_factories(factories)
        .with_snapshot_enabled(None)
        .sync()
        .await?;

    /*
    The subscribe method listens for new blocks and fetches
    all logs matching any `sync_events()` specified by the AMM variants in the state space.
    Under the hood, this method applies all state changes to any affected AMMs and returns a Vec of
    addresses, indicating which AMMs have been updated.
    */
    let mut stream = state_space_manager.subscribe().await?;
    while let Some(updated_amms) = stream.next().await {
        if let Ok(amms) = updated_amms {
            println!("Updated AMMs: {:?}", amms);
        }
    }

    Ok(())
}
