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
    crypto::{
        Hash, address_inclusion_filter::AddressInclusionFilter, keys::Public,
        merkle_tree::MerkleTree, randomx_use_full_mode,
    },
    economics::{EXPIRATION_TIME, calculate_dev_fee, get_block_reward},
    full_node::node_state::ChainEvent,
    to_snap,
};
use std::{
    env::args,
    fs::{self, File},
    io::Write,
    process::exit,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{RwLock, broadcast, mpsc},
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

pub fn normalize_difficulty(target: &[u8; 32]) -> f64 {
    // find first non-zero byte
    let mut i = 0;
    while i < 32 && target[i] == 0 {
        i += 1;
    }

    if i == 32 {
        return f64::INFINITY;
    }

    // read top 8 bytes for mantissa
    let mut buf = [0u8; 8];
    let len = (32 - i).min(8);
    buf[..len].copy_from_slice(&target[i..i + len]);

    let mantissa = u64::from_be_bytes(buf) as f64;

    // exponent in bits
    let exp = ((32 - i) * 8) as i32;

    // max target is all 0xff
    let max_mantissa = u64::MAX as f64;
    let max_exp = 256i32;

    let target_f = mantissa * 2f64.powi(exp - 64);
    let max_f = max_mantissa * 2f64.powi(max_exp - 64);

    max_f / target_f
}

#[derive(Copy, Clone, Debug)]
struct PoolInfo {
    pool_difficulty: [u8; 32],
}

const DEFAULT_CONFIG: &str = "[node]
address = \"127.0.0.1:3003\"

[miner]
public = \"<your public wallet address>\"

[threads]
count = 1";

async fn init_client_pool(client: &Client, miner: Public) -> Result<PoolInfo, anyhow::Error> {
    let mut client = client.stream.lock().await;
    let mut pool_difficulty = [0u8; 32];
    client.write_all(miner.dump_buf()).await?;
    client.read_exact(&mut pool_difficulty).await?;

    Ok(PoolInfo { pool_difficulty })
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut config_path = "./miner.toml";

    let args: Vec<String> = args().into_iter().collect();
    let mut full_dataset = true;
    let mut is_pool = false;
    for (place, arg) in args.iter().enumerate() {
        if arg == "--config" && args.get(place + 1).is_some() {
            config_path = &args[place + 1];
        }
        if arg == "--no-dataset" {
            full_dataset = false;
        }
        if arg == "--pool" {
            is_pool = true;
        }
    }
    if full_dataset {
        randomx_use_full_mode();
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
    let submission_client = Arc::new(Client::connect(node_address.parse().unwrap()).await?);
    let event_client = Client::connect(node_address.parse().unwrap()).await?;
    let job_client = Arc::new(Client::connect(node_address.parse().unwrap()).await?);

    let pool_info = if is_pool {
        init_client_pool(&*submission_client, miner_pub).await?;
        init_client_pool(&event_client, miner_pub).await?;
        let pool_info = init_client_pool(&*job_client, miner_pub).await?;
        println!(
            "Pool INFO:\nDifficulty: {}",
            normalize_difficulty(&pool_info.pool_difficulty)
        );
        Some(pool_info)
    } else {
        None
    };

    // A task for block submissions, submitted via MPSC and a new block transmitted to all mining threads via a broadcast.
    let (submission_tx, mut submission_rx) = mpsc::channel::<Block>(1);

    let (job_tx, _) = broadcast::channel::<Block>(64);

    let hash_counter = Arc::new(AtomicU64::new(0));

    let global_job_id = Arc::new(AtomicU64::new(0));

    // Create mining threads
    for i in 0..thread_count {
        println!("[THREAD {}] Starting miner", i + 1);
        let mut job_rx = job_tx.subscribe();
        let submission_tx = submission_tx.clone();
        let hash_counter = hash_counter.clone();
        let global_job_id = global_job_id.clone();
        thread::spawn(move || {
            let mut rng = rng();
            let mut local_job_id = 0;
            let mut current_block: Option<Block> = None;

            loop {
                if let Err(e) = (|| {
                    // Check for new jobs and wait until we're synchronized
                    let current_global_job = global_job_id.load(Ordering::Relaxed);

                    // If we don't have a block yet or we're behind, wait for new job
                    while current_block.is_none() || current_global_job > local_job_id {
                        match job_rx.blocking_recv() {
                            Ok(job) => {
                                local_job_id += 1;
                                current_block = Some(job);
                                break;
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                println!(
                                    "[THREAD {i}] Warning: Lagged by {skipped} jobs, catching up..."
                                );
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                return Err(anyhow!("Job channel closed"));
                            }
                        }
                    }

                    // Only mine if we're synchronized with the global job ID
                    if global_job_id.load(Ordering::Relaxed) != local_job_id {
                        thread::sleep(Duration::from_millis(10));
                        return Ok(());
                    }

                    let block = current_block.as_mut().unwrap();
                    block.timestamp = chrono::Utc::now().timestamp() as u64;

                    let mut removed_txs = false;
                    // 10s expiration margin
                    block.transactions.retain(|tx| {
                        let expired = tx.timestamp + EXPIRATION_TIME + 10
                            < chrono::Utc::now().timestamp() as u64;
                        if expired {
                            removed_txs = true;
                        }
                        !expired
                    });

                    if removed_txs {
                        block.meta.merkle_tree_root = MerkleTree::build(
                            &block
                                .transactions
                                .iter()
                                .map(|tx| tx.transaction_id.unwrap())
                                .collect::<Vec<TransactionId>>(),
                        )
                        .root_hash();
                        block.meta.address_inclusion_filter =
                            AddressInclusionFilter::create_filter(&block.transactions)?;
                    }

                    block.nonce = rng.random();
                    block.meta.hash = Some(Hash::new(&block.get_hashing_buf()?));

                    if is_pool {
                        if BigUint::from_bytes_be(&pool_info.unwrap().pool_difficulty)
                            > BigUint::from_bytes_be(&*block.meta.hash.unwrap())
                            && global_job_id.load(Ordering::Relaxed) == local_job_id
                        {
                            println!(
                                "[THREAD {i}] Found share {}",
                                block.meta.hash.unwrap().dump_base36()
                            );
                            submission_tx.blocking_send(block.clone())?;
                        }
                    } else {
                        if BigUint::from_bytes_be(&block.meta.block_pow_difficulty)
                            > BigUint::from_bytes_be(&*block.meta.hash.unwrap())
                            && global_job_id.load(Ordering::Relaxed) == local_job_id
                        {
                            println!(
                                "[THREAD {i}] Found block {}",
                                block.meta.hash.unwrap().dump_base36()
                            );
                            submission_tx.blocking_send(block.clone())?;
                        }
                    }

                    hash_counter.fetch_add(1, Ordering::Relaxed);

                    Ok::<(), anyhow::Error>(())
                })() {
                    println!("[THREAD {i}] Mining error: {e}")
                }
            }
        });
    }

    // Give threads time to subscribe before starting job refresh
    tokio::time::sleep(Duration::from_millis(100)).await;

    let global_job_id_for_refresh = global_job_id.clone();
    let _job_task = {
        tokio::spawn(async move {
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
                    tx.timestamp + 5 < EXPIRATION_TIME + chrono::Utc::now().timestamp() as u64
                }); // Add a 5s anti expiration buffer
                Ok(mempool)
            }

            let is_refreshing = Arc::new(RwLock::new(false));
            // We don't really care about what the event is because, it always requires recomputing the block
            let refresh_block = move |event: Option<ChainEvent>| {
                let client = job_client.clone();
                let job_tx = job_tx.clone();
                let is_refreshing: Arc<RwLock<bool>> = is_refreshing.clone();
                let global_job_id = global_job_id_for_refresh.clone();
                tokio::spawn(async move {
                    if *is_refreshing.read().await {
                        return;
                    }

                    *is_refreshing.write().await = true;
                    if let Err(e) = async move {
                        if is_pool {
                            if let Some(event) = event
                                && let ChainEvent::Block { block: job } = event
                            {
                                let new_job_id = global_job_id.fetch_add(1, Ordering::Relaxed) + 1;
                                println!("[JOB] Received new job (ID: {new_job_id})");
                                job_tx.send(job)?;
                            }
                            return Ok(());
                        }

                        let block =
                            build_block(&*client, &get_current_mempool(&*client).await?, miner_pub)
                                .await?;

                        // Increment global job ID BEFORE sending the new job
                        let new_job_id = global_job_id.fetch_add(1, Ordering::Relaxed) + 1;
                        println!("[JOB] Received new job (ID: {new_job_id})");

                        job_tx.send(block)?;

                        Ok::<(), anyhow::Error>(())
                    }
                    .await
                    {
                        println!("[JOB] Error {e}");
                        exit(1);
                    }
                    *is_refreshing.write().await = false;
                });
            };

            // Initial block refresh
            if !is_pool {
                refresh_block(None)
            };

            if let Err(e) = event_client
                .convert_to_event_listener(|event| {
                    refresh_block(Some(event));
                })
                .await
            {
                println!("[JOB] Error: {:?}", e);
                exit(2);
            }
        })
    };

    let _hash_rate_task = tokio::spawn(async move {
        loop {
            if let Err(e) = async {
                let last = chrono::Utc::now().timestamp_millis() as f64;
                sleep(Duration::from_secs(3)).await;

                let hashes = hash_counter.swap(0, Ordering::Relaxed) as f64;
                let delta = chrono::Utc::now().timestamp_millis() as f64 - last;
                let (display_rate, units) = format_hash_rate((hashes / delta) * 1000f64);
                println!("[STATUS] Hash rate: {} {}", display_rate, units);
                Ok::<(), anyhow::Error>(())
            }
            .await
            {
                println!("[STATUS] Error: {:?}", e);
            }
        }
    });

    let submission_task = tokio::spawn(async move {
        loop {
            if let Err(e) = async {
                let candidate = submission_rx.recv().await;
                if let Some(candidate) = candidate {
                    // Don't increment global_job_id here - it's already incremented by refresh_block
                    // which gets called when the block is accepted
                    submission_client.submit_block(candidate).await??;
                    if !is_pool {
                        let block_reward = get_block_reward(
                            submission_client.get_height().await?.saturating_sub(1),
                        );
                        println!(
                            "[SUBMISSIONS] Block validated! Miner rewarded {} SNAP",
                            to_snap(block_reward - calculate_dev_fee(block_reward))
                        );
                    } else {
                        println!("[SUBMISSIONS] Share validated! Miner awarded share");
                    }
                } else {
                    eprint!("[SUBMISSIONS] All miner threads died!");
                }

                Ok::<(), anyhow::Error>(())
            }
            .await
            {
                println!("[SUBMISSIONS] Error: {:?}", e);
            }
        }
    });

    submission_task.await?;

    Ok(())
}
