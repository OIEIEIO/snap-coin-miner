use anyhow::anyhow;
use config::Config;
use num_bigint::BigUint;
use rand::{Rng, rng};
use snap_coin::{
    api::client::Client,
    blockchain_data_provider::{BlockchainDataProvider, BlockchainDataProviderError},
    build_block,
    core::{
        block::{Block, MAX_TRANSACTIONS_PER_BLOCK},
        transaction::{Transaction, TransactionId},
        utils::slice_vec,
    },
    crypto::{Hash, keys::Public},
    economics::{EXPIRATION_TIME, get_block_reward},
    to_snap,
};
use std::{
    env::args,
    fs::{self, File},
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc},
    time::sleep,
};

fn format_hash_rate(hps: f64) -> (f64, &'static str) {
    const UNITS: [&str; 5] = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s"];

    let mut rate = hps;
    let mut unit = 0;

    while rate >= 1000.0 && unit < UNITS.len() - 1 {
        rate /= 1000.0;
        unit += 1;
    }

    (rate, UNITS[unit])
}

const BATCH_SIZE: u64 = 20;

const DEFAULT_CONFIG: &str = "[node]
address = \"127.0.0.1:3003\"

[miner]
public = \"<your public wallet address>\"

[threads]
count = 1";

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut config_path = "./miner.toml";

    let args: Vec<String> = args().into_iter().collect();
    for (place, arg) in args.iter().enumerate() {
        if arg == "--config" && args.get(place + 1).is_some() {
            config_path = &args[place + 1];
        }
    }

    if !fs::exists(config_path).is_ok_and(|exists| exists == true) {
        File::create(config_path)?.write(DEFAULT_CONFIG.as_bytes())?;
        return Err(anyhow!(
            "Created new config file: {}. Please replace <your public wallet address> in the config with your real miner address",
            config_path
        ));
    }

    let settings = Config::builder()
        .add_source(config::File::with_name("miner.toml"))
        .build()?;

    let node_address: String = settings.get("node.address")?;
    let public_key_base36: String = settings.get("miner.public")?;
    let thread_count: i32 = settings.get("threads.count")?;
    let thread_count = if thread_count == -1 {
        thread::available_parallelism()?.get() as i32
    } else {
        thread_count
    };

    let miner_pub = Public::new_from_base36(&public_key_base36).expect("Invalid public key");
    let client = Arc::new(Client::connect(node_address.parse().unwrap()).await?);

    // A task for block submissions, submitted via MPSC and a new block transmitted to all mining threads via a broadcast.
    let (submission_tx, mut submission_rx) = mpsc::channel::<Block>(1);

    let (job_tx, _) = broadcast::channel::<Block>(1);

    let hash_counter = Arc::new(AtomicU64::new(0));

    // Create mining threads
    for i in 0..thread_count {
        println!("[THREAD {i}] Starting miner");
        let mut job_rx = job_tx.subscribe();
        let submission_tx = submission_tx.clone();
        let hash_counter = hash_counter.clone();
        thread::spawn(move || {
            // At startup wait for block thread to create a block
            let mut current_block = loop {
                match job_rx.blocking_recv() {
                    Ok(v) => break v,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            };

            let mut rng = rng();
            loop {
                if let Err(e) = (|| {
                    if !job_rx.is_empty() {
                        current_block = job_rx.blocking_recv()?;
                    }
                    current_block.timestamp = chrono::Utc::now().timestamp() as u64;

                    for _ in 0..BATCH_SIZE {
                        current_block.nonce = rng.random();
                        current_block.meta.hash =
                            Some(Hash::new(&current_block.get_hashing_buf()?));

                        if BigUint::from_bytes_be(&current_block.meta.block_pow_difficulty)
                            > BigUint::from_bytes_be(&*current_block.meta.hash.unwrap())
                        {
                            println!(
                                "[THREAD {i}] Found block {}",
                                current_block.meta.hash.unwrap().dump_base36()
                            );
                            submission_tx.blocking_send(current_block.clone())?;
                            break;
                        }
                    }
                    hash_counter.fetch_add(BATCH_SIZE, Ordering::Relaxed);

                    Ok::<(), anyhow::Error>(())
                })() {
                    println!("[THREAD {i}] Mining error: {e}")
                }
            }
        });
    }

    let _job_task = {
        let client = client.clone();
        tokio::spawn(async move {
            async fn get_diffs(
                client: &Client,
            ) -> Result<([u8; 32], [u8; 32]), BlockchainDataProviderError> {
                Ok((
                    client.get_block_difficulty().await?,
                    client.get_transaction_difficulty().await?,
                ))
            }

            async fn get_current_mempool(
                client: &Client,
            ) -> Result<Vec<Transaction>, BlockchainDataProviderError> {
                let mut mempool = slice_vec(
                    &client.get_mempool().await?,
                    0,
                    MAX_TRANSACTIONS_PER_BLOCK - 1,
                )
                .to_vec();
                mempool.retain(|tx| {
                    tx.timestamp + 3 + 5 < EXPIRATION_TIME + chrono::Utc::now().timestamp() as u64
                }); // Add a 10s anti expiration buffer
                Ok(mempool)
            }

            let mut last_block_diffs = ([0u8; 32], [0u8; 32]);
            let mut last_mempool: Vec<Transaction> = vec![];
            loop {
                if let Err(e) = async {
                    let last = chrono::Utc::now().timestamp_millis() as f64;
                    sleep(Duration::from_secs(3)).await;
                    let current_block_diffs = get_diffs(&*client).await?;
                    let current_mempool = get_current_mempool(&*client).await?;

                    if last_block_diffs != current_block_diffs
                        || current_mempool
                            .iter()
                            .map(|tx| tx.transaction_id)
                            .collect::<Vec<Option<TransactionId>>>()
                            != last_mempool
                                .iter()
                                .map(|tx| tx.transaction_id)
                                .collect::<Vec<Option<TransactionId>>>()
                    {
                        last_block_diffs = current_block_diffs;
                        last_mempool = current_mempool.clone();
                        let block = build_block(&*client, &current_mempool, miner_pub).await?;
                        job_tx.send(block)?;
                    }
                    let hashes = hash_counter.swap(0, Ordering::Relaxed) as f64;
                    let delta = chrono::Utc::now().timestamp_millis() as f64 - last;
                    let (display_rate, units) = format_hash_rate((hashes / delta) * 1000f64);
                    println!("[STATUS] Hash rate: {} {}", display_rate, units);
                    Ok::<(), anyhow::Error>(())
                }
                .await
                {
                    println!("[JOB] Error: {e}");
                }
            }
        })
    };

    let submission_task = tokio::spawn(async move {
        loop {
            if let Err(e) = async {
                let candidate = submission_rx.recv().await;
                if let Some(candidate) = candidate {
                    client.submit_block(candidate).await??;
                    println!(
                        "[SUBMISSIONS] Block validated! Miner rewarded {} SNAP",
                        to_snap(get_block_reward(
                            client.get_height().await?.saturating_sub(1)
                        ))
                    );
                } else {
                    eprint!("[SUBMISSIONS] All miner threads died!");
                }

                Ok::<(), anyhow::Error>(())
            }
            .await
            {
                println!("[SUBMISSIONS] Error: {e}");
            }
        }
    });

    submission_task.await?;

    Ok(())
}
