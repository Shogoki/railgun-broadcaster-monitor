use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use anyhow::Result;
use tokio::sync::broadcast;

use crate::{
    config::{Config, SourceKind},
    db::Db,
    market::Market,
    rpc::RpcPool,
};

pub struct App {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub rpcs: HashMap<u64, Arc<RpcPool>>,
    /// Change notifications ("relays", "requests", "health", "fees", "market") for SSE.
    pub events: broadcast::Sender<&'static str>,
    pub market: Arc<RwLock<Market>>,
    /// The live log source this process runs with.
    pub source: SourceKind,
    /// Process start, microseconds since the epoch (to tell "nothing yet" from "nothing, ever").
    pub started_us: i64,
}

impl App {
    pub fn new(cfg: Config, db: Db, source: SourceKind) -> Result<Self> {
        let mut rpcs = HashMap::new();
        for c in &cfg.chains {
            rpcs.insert(c.id, Arc::new(RpcPool::new(c.id, &c.rpcs, &cfg.rpc)?));
        }
        let (events, _) = broadcast::channel(64);
        let market = Arc::new(RwLock::new(Market::new(&cfg)));
        Ok(Self { cfg: Arc::new(cfg), db, rpcs, events, market, source, started_us: crate::db::now_us() })
    }

    pub fn notify(&self, topic: &'static str) {
        let _ = self.events.send(topic);
    }
}
