use anyhow::Result;
use snap_coin::{
    api::client::Client,
    blockchain_data_provider::BlockchainDataProviderError,
    build_block,
    core::{
        block::{Block, MAX_TRANSACTIONS_PER_BLOCK},
        transaction::Transaction,
        utils::slice_vec,
    },
    crypto::keys::Public,
    economics::EXPIRATION_TIME,
    full_node::node_state::ChainEvent,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{RwLock, broadcast, mpsc};

use crate::stats::StatEvent;

pub struct JobManager {
    job_client: Arc<Client>,
    job_tx: broadcast::Sender<Block>,
    global_job_id: Arc<AtomicU64>,
    miner_pub: Public,
    is_pool: bool,
    stat_tx: mpsc::UnboundedSender<StatEvent>,
    shutdown_rx: broadcast::Receiver<()>,
}

impl JobManager {
    pub fn new(
        job_client: Arc<Client>,
        job_tx: broadcast::Sender<Block>,
        global_job_id: Arc<AtomicU64>,
        miner_pub: Public,
        is_pool: bool,
        stat_tx: mpsc::UnboundedSender<StatEvent>,
        shutdown_rx: broadcast::Receiver<()>,
    ) -> Self {
        Self {
            job_client,
            job_tx,
            global_job_id,
            miner_pub,
            is_pool,
            stat_tx,
            shutdown_rx,
        }
    }

    pub async fn start(self, event_client: Client) -> Result<()> {
        let is_refreshing = Arc::new(RwLock::new(false));

        // Initial block refresh for solo mining
        if !self.is_pool {
            self.refresh_block(None, is_refreshing.clone());
        }

        // Listen for events and refresh on each one
        event_client
            .convert_to_event_listener(
                |event| {
                    self.refresh_block(Some(event), is_refreshing.clone());
                },
                Some(self.shutdown_rx.resubscribe()),
            )
            .await?;

        Ok(())
    }

    fn refresh_block(&self, event: Option<ChainEvent>, is_refreshing: Arc<RwLock<bool>>) {
        let client = self.job_client.clone();
        let job_tx = self.job_tx.clone();
        let global_job_id = self.global_job_id.clone();
        let miner_pub = self.miner_pub;
        let is_pool = self.is_pool;
        let stat_tx = self.stat_tx.clone();

        tokio::spawn(async move {
            // Prevent concurrent refreshes
            if *is_refreshing.read().await {
                return;
            }

            *is_refreshing.write().await = true;

            let stat_tx_clone = stat_tx.clone();
            if let Err(e) = async move {
                // Pool mode: use job sent by pool
                if is_pool {
                    if let Some(event) = event
                        && let ChainEvent::Block { block: job } = event
                    {
                        let new_job_id = global_job_id.fetch_add(1, Ordering::Relaxed) + 1;
                        stat_tx.send(StatEvent::NewJob(new_job_id)).ok();
                        job_tx.send(job)?;
                    }
                    return Ok(());
                }

                // Solo mode: build our own block
                let block = build_block(
                    &*client,
                    &Self::get_current_mempool(&*client).await?,
                    miner_pub,
                )
                .await?;

                let new_job_id = global_job_id.fetch_add(1, Ordering::Relaxed) + 1;
                stat_tx.send(StatEvent::NewJob(new_job_id)).ok();

                job_tx.send(block)?;

                Ok::<(), anyhow::Error>(())
            }
            .await
            {
                stat_tx_clone
                    .send(StatEvent::Event(format!("Job error: {}", e)))
                    .ok();
            }

            *is_refreshing.write().await = false;
        });
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

        // Filter out transactions that are close to expiring (5s buffer)
        mempool.retain(|tx| {
            tx.timestamp + 5 < EXPIRATION_TIME + chrono::Utc::now().timestamp() as u64
        });

        Ok(mempool)
    }
}
