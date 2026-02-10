use anyhow::anyhow;
use core_affinity::CoreId;
use rand::random;
use snap_coin::{
    core::{block::Block, transaction::TransactionId},
    crypto::{Hash, address_inclusion_filter::AddressInclusionFilter, merkle_tree::MerkleTree},
    economics::EXPIRATION_TIME,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};

use crate::pool::PoolInfo;
use crate::stats::StatEvent;

pub struct MiningThread;

impl MiningThread {
    pub fn spawn(
        thread_id: i32,
        mut job_rx: broadcast::Receiver<Block>,
        submission_tx: mpsc::UnboundedSender<Block>,
        hash_counter: Arc<AtomicU64>,
        global_job_id: Arc<AtomicU64>,
        pool_info: Option<PoolInfo>,
        is_pool: bool,
        stat_tx: mpsc::UnboundedSender<StatEvent>,
        mut shutdown: broadcast::Receiver<()>,
        cpu_core: Option<CoreId>,
    ) {
        thread::spawn(move || {
            let mut local_job_id = 0;
            let mut current_block: Option<Block> = None;
            let mut thread_hashes = 0u64;
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let shutdown_flag_clone = shutdown_flag.clone();

            core_affinity::set_for_current(cpu_core.expect("No core affinity found!"));

            let stat_tx_clone = stat_tx.clone();
            thread::spawn(move || {
                let _ = shutdown.blocking_recv();
                stat_tx_clone
                    .send(StatEvent::Event(format!("Requested thread shutdown {thread_id}")))
                    .ok();
                shutdown_flag_clone.store(true, Ordering::Relaxed);
            });

            loop {
                // Fast shutdown check - just atomic load, no syscalls
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }

                if let Err(_) = Self::mine_iteration(
                    thread_id,
                    &mut job_rx,
                    &submission_tx,
                    &hash_counter,
                    &global_job_id,
                    &mut local_job_id,
                    &mut current_block,
                    pool_info,
                    is_pool,
                    &stat_tx,
                    &mut thread_hashes,
                    &shutdown_flag,
                ) {
                    break;
                }
            }
        });
    }

    fn mine_iteration(
        thread_id: i32,
        job_rx: &mut broadcast::Receiver<Block>,
        submission_tx: &mpsc::UnboundedSender<Block>,
        hash_counter: &Arc<AtomicU64>,
        global_job_id: &Arc<AtomicU64>,
        local_job_id: &mut u64,
        current_block: &mut Option<Block>,
        pool_info: Option<PoolInfo>,
        is_pool: bool,
        stat_tx: &mpsc::UnboundedSender<StatEvent>,
        thread_hashes: &mut u64,
        shutdown_flag: &Arc<AtomicBool>,
    ) -> Result<(), anyhow::Error> {
        let current_global_job = global_job_id.load(Ordering::Relaxed);

        // Wait for a new job if we don't have one or are behind
        while current_block.is_none() || current_global_job > *local_job_id {
            if shutdown_flag.load(Ordering::Relaxed) {
                return Err(anyhow!("Requested thread shutdown {thread_id}"));
            }

            match job_rx.try_recv() {
                Ok(job) => {
                    *local_job_id += 1;
                    *current_block = Some(job);
                    current_block.as_mut().unwrap().nonce = random();
                    break;
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    stat_tx
                        .send(StatEvent::Event(format!(
                            "Thread {} lagged by {} jobs",
                            thread_id + 1,
                            skipped
                        )))
                        .ok();
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(anyhow!("Job channel closed"));
                }
            }
        }

        // Only mine if synchronized with global job ID
        if global_job_id.load(Ordering::Relaxed) != *local_job_id {
            thread::sleep(Duration::from_millis(10));
            return Ok(());
        }

        let block = current_block.as_mut().unwrap();
        block.timestamp = chrono::Utc::now().timestamp() as u64;

        // Remove expired transactions with 10s margin
        let mut removed_txs = false;
        block.transactions.retain(|tx| {
            let expired =
                tx.timestamp + EXPIRATION_TIME + 10 < chrono::Utc::now().timestamp() as u64;
            if expired {
                removed_txs = true;
            }
            !expired
        });

        // Update merkle tree and filter if transactions were removed
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

        // Try a new nonce and hash the block
        block.nonce += 1;
        block.meta.hash = Some(Hash::new(&block.get_hashing_buf()?));

        // Increment hash counter and thread-specific counter
        hash_counter.fetch_add(1, Ordering::Relaxed);
        *thread_hashes += 1;

        // Report thread stats every 100 hashes
        if *thread_hashes % 100 == 0 {
            stat_tx
                .send(StatEvent::ThreadHash(thread_id as usize, *thread_hashes))
                .ok();
            *thread_hashes = 0; // Reset
        }

        // Check if the hash meets the difficulty target
        if is_pool {
            if pool_info.unwrap().pool_difficulty > *block.meta.hash.unwrap()
                && global_job_id.load(Ordering::Relaxed) == *local_job_id
            {
                stat_tx
                    .send(StatEvent::Event(format!(
                        "Thread {} found share: {}",
                        thread_id + 1,
                        block.meta.hash.unwrap().dump_base36()
                    )))
                    .ok();
                submission_tx
                    .send(block.clone())
                    .map_err(|e| anyhow!("Failed to send submission: {}", e))?;
            }
        } else {
            if block.meta.block_pow_difficulty > *block.meta.hash.unwrap()
                && global_job_id.load(Ordering::Relaxed) == *local_job_id
            {
                stat_tx
                    .send(StatEvent::Event(format!(
                        "Thread {} found block: {}",
                        thread_id + 1,
                        block.meta.hash.unwrap().dump_base36()
                    )))
                    .ok();
                submission_tx
                    .send(block.clone())
                    .map_err(|e| anyhow!("Failed to send submission: {}", e))?;
            }
        }

        Ok(())
    }
}
