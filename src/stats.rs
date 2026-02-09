use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::sleep};

#[derive(Debug, Clone)]
pub enum StatEvent {
    HashRate(f64),
    ThreadHash(usize, u64),
    ShareAccepted,
    ShareRejected,
    BlockAccepted,
    BlockRejected,
    NewJob(u64),
    Event(String),
}

pub struct MinerStats {
    hash_counter: Arc<AtomicU64>,
    stat_tx: mpsc::UnboundedSender<StatEvent>,
}

impl MinerStats {
    pub fn new(
        hash_counter: Arc<AtomicU64>,
        stat_tx: mpsc::UnboundedSender<StatEvent>,
    ) -> Self {
        Self {
            hash_counter,
            stat_tx,
        }
    }

    pub fn start(self) {
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.measure_hash_rate().await {
                    self.stat_tx.send(StatEvent::Event(format!("Stats error: {}", e))).ok();
                }
            }
        });
    }

    async fn measure_hash_rate(&self) -> Result<(), anyhow::Error> {
        let last = chrono::Utc::now().timestamp_millis() as f64;
        sleep(Duration::from_secs(1)).await;

        let hashes = self.hash_counter.swap(0, Ordering::Relaxed) as f64;
        let delta = chrono::Utc::now().timestamp_millis() as f64 - last;
        let hash_rate = (hashes / delta) * 1000.0;
        
        self.stat_tx.send(StatEvent::HashRate(hash_rate)).ok();
        
        Ok(())
    }
}