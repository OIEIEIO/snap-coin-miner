use anyhow::Result;
use snap_coin::{api::client::Client, crypto::keys::Public};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Copy, Clone, Debug)]
pub struct PoolInfo {
    pub pool_difficulty: [u8; 32],
}

pub async fn initialize_pool_connections(
    submission_client: &Client,
    event_client: &Client,
    job_client: &Client,
    miner: Public,
) -> Result<PoolInfo> {
    // Initialize all three clients with the pool
    init_client_pool(submission_client, miner).await?;
    init_client_pool(event_client, miner).await?;
    let pool_info = init_client_pool(job_client, miner).await?;
    Ok(pool_info)
}

async fn init_client_pool(client: &Client, miner: Public) -> Result<PoolInfo> {
    let mut client = client.stream.lock().await;
    let mut pool_difficulty = [0u8; 32];
    
    // Send miner public key and receive pool difficulty
    client.write_all(miner.dump_buf()).await?;
    client.read_exact(&mut pool_difficulty).await?;

    Ok(PoolInfo { pool_difficulty })
}