//! Waku fee listener (light client: filter + store, via railgun-rust `waku-relay`) and
//! the market sampler that persists summaries.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use ruint::aliases::U256;
use rusqlite::params;

use crate::{
    app::App,
    db::now_us,
    market::{Offer, TokenView, decode_fee_message},
    units::parse_uint,
};

/// `/railgun/v2/0-<chainId>-fees/json` → chain id.
pub fn fees_topic_chain(topic: &str) -> Option<u64> {
    topic.strip_prefix("/railgun/v2/0-")?.strip_suffix("-fees/json")?.parse().ok()
}

/// Feed one Waku message into the market state. Returns the chain if it was a fee message.
pub fn handle_message(app: &App, content_topic: &str, payload: &[u8]) -> Option<u64> {
    let chain = fees_topic_chain(content_topic)?;
    let mut m = app.market.write().unwrap();
    m.status.messages += 1;
    m.status.last_message_us = Some(now_us());
    match decode_fee_message(payload) {
        Ok((body, valid)) => {
            if !valid {
                m.status.invalid += 1;
            }
            m.upsert(chain, Offer::from_body(body, valid, now_us()));
            Some(chain)
        }
        Err(_) => {
            m.status.invalid += 1;
            None
        }
    }
}

#[cfg(feature = "waku")]
pub async fn listener_loop(app: Arc<App>) {
    loop {
        if let Err(e) = listen_once(&app).await {
            tracing::warn!(error = %e, "waku listener stopped; reconnecting in 30 s");
            app.market.write().unwrap().status.error = Some(e.to_string());
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

#[cfg(feature = "waku")]
async fn listen_once(app: &App) -> Result<()> {
    use waku_relay::{
        ContentTopic,
        client::{AdditionalPeer, Client, ClientConfig},
    };
    let w = &app.cfg.waku;
    let cfg = ClientConfig {
        cluster_id: Some(5),
        shard_id: Some(1),
        max_peers: Some(w.max_peers.max(1)),
        direct_peers: w
            .direct_peers
            .iter()
            .map(|p| AdditionalPeer { peer_id: p.peer_id.clone(), addrs: p.addrs.clone() })
            .collect(),
        dns_enr_trees: w.dns_enr_trees.clone(),
        ..Default::default()
    };
    let client = Client::new(&cfg)?;
    let topics: Vec<String> = app.cfg.chains.iter().map(|c| ContentTopic::fees_topic(c.id)).collect();
    let mut rx = client.subscribe_with_fee_history(topics).await?;
    tracing::info!(chains = app.cfg.chains.len(), "waku: subscribed to fee topics");
    app.market.write().unwrap().status.error = None;
    let mut stats = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { anyhow::bail!("waku subscription closed") };
                handle_message(app, &msg.content_topic, &msg.payload);
            }
            _ = stats.tick() => {
                let peers = client.peer_stats().connected_peers.len();
                app.market.write().unwrap().status.connected_peers = peers;
            }
        }
    }
}

#[cfg(not(feature = "waku"))]
pub async fn listener_loop(app: Arc<App>) {
    app.market.write().unwrap().status.error = Some("built without the `waku` feature".into());
}

/// Our currently published rates (from our own `our fees` log lines), if fresh.
pub fn our_logged_rates(c: &rusqlite::Connection, max_age_us: i64) -> Result<HashMap<(u64, String), U256>> {
    let mut st = c.prepare("SELECT chain_id, token, rate FROM our_fees WHERE ts >= ?1")?;
    let rows = st.query_map([now_us() - max_age_us], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (chain, token, rate) = r?;
        if let Some(rate) = parse_uint(&rate) {
            out.insert((chain as u64, token), rate);
        }
    }
    Ok(out)
}

pub const OUR_RATE_MAX_AGE_US: i64 = 300 * 1_000_000;

/// Inputs for the market views that live in the database.
#[derive(Debug, Default)]
pub struct MarketInputs {
    /// Our currently published rates from our own logs (fresh only).
    pub ours: HashMap<(u64, String), U256>,
    /// Latest native/USD per chain.
    pub native_usd: HashMap<u64, f64>,
}

pub fn market_inputs(c: &rusqlite::Connection) -> Result<MarketInputs> {
    let ours = our_logged_rates(c, OUR_RATE_MAX_AGE_US)?;
    let mut st = c.prepare(
        "SELECT p.chain_id, p.usd FROM prices p
         JOIN (SELECT chain_id, max(ts) AS ts FROM prices GROUP BY chain_id) l ON l.chain_id = p.chain_id AND l.ts = p.ts",
    )?;
    let native_usd = st
        .query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, f64>(1)?)))?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(MarketInputs { ours, native_usd })
}

/// Market-price reference rate (token base units per 1e18 wei) for tokens we can price
/// without our own offer: the wrapped native token (1:1) and USD stablecoins.
fn market_reference(app: &App, inputs: &MarketInputs, chain: u64, token: &str) -> Option<f64> {
    let c = app.cfg.chain(chain)?;
    let t = app.cfg.token(chain, token)?;
    if t.symbol == format!("W{}", c.native_symbol) {
        return Some(1e18);
    }
    if t.stable {
        return inputs.native_usd.get(&chain).map(|p| p * 10f64.powi(t.decimals as i32));
    }
    None
}

/// Compute views for every configured chain/token.
pub fn all_views(app: &App, inputs: &MarketInputs) -> Vec<TokenView> {
    let now_ms = now_us() / 1000;
    let m = app.market.read().unwrap();
    let mut out = Vec::new();
    for c in &app.cfg.chains {
        for t in m.tokens(&app.cfg, c.id) {
            let ours = inputs.ours.get(&(c.id, t.clone())).copied();
            let reference = market_reference(app, inputs, c.id, &t);
            out.push(m.view(&app.cfg, c.id, &t, now_ms, ours, reference));
        }
    }
    out
}

/// Periodically persist market summaries + (less often) per-broadcaster samples.
pub async fn sampler_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(app.cfg.waku.sample_secs.max(30)));
    tick.tick().await; // skip the immediate tick: give the listener time to fill up
    let fee_every = app.cfg.waku.fee_sample_secs.max(60) as i64 * 1_000_000;
    let mut last_fee_sample = 0i64;
    loop {
        tick.tick().await;
        let with_fees = now_us() - last_fee_sample >= fee_every;
        match sample_once(&app, with_fees).await {
            Ok(true) if with_fees => last_fee_sample = now_us(),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "market sample failed"),
        }
    }
}

/// Returns whether anything was written (false while no offers are known yet).
pub async fn sample_once(app: &App, with_fee_samples: bool) -> Result<bool> {
    let inputs = app.db.read(market_inputs).await?;
    let now = now_us();
    app.market.write().unwrap().prune(now / 1000);
    let views = all_views(app, &inputs);
    if views.iter().all(|v| v.n_broadcasters == 0) {
        return Ok(false);
    }
    let offers: Vec<(u64, Offer)> = if with_fee_samples {
        let m = app.market.read().unwrap();
        m.offers
            .iter()
            .flat_map(|(c, by)| by.values().filter(|o| o.is_live(now / 1000)).map(move |o| (*c, o.clone())))
            .collect()
    } else {
        Vec::new()
    };
    app.db
        .write(move |c| {
            let tx = c.transaction()?;
            for v in &views {
                if v.n_broadcasters == 0 && v.our_rate.is_none() {
                    continue;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO market_snapshots(ts, chain_id, token, our_rate, our_rank, n_broadcasters, cheapest, pool_size, in_pool, picked, eligible)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        now,
                        v.chain_id as i64,
                        v.token,
                        v.our_rate,
                        v.our_rank.map(|r| r as i64),
                        v.n_broadcasters as i64,
                        v.cheapest,
                        v.pool_size as i64,
                        v.in_pool,
                        v.picked,
                        v.our_eligibility.map(|e| e == crate::market::Eligibility::Eligible)
                    ],
                )?;
            }
            for (chain, o) in &offers {
                tx.execute(
                    "INSERT INTO broadcasters(railgun_address, identifier, first_seen, last_seen) VALUES(?1, ?2, ?3, ?3)
                     ON CONFLICT(railgun_address) DO UPDATE SET last_seen=excluded.last_seen,
                        identifier=coalesce(excluded.identifier, identifier)",
                    params![o.railgun_address, o.identifier, now],
                )?;
                let id: i64 = tx.query_row(
                    "SELECT id FROM broadcasters WHERE railgun_address=?1",
                    [&o.railgun_address],
                    |r| r.get(0),
                )?;
                for (token, rate) in &o.fees {
                    tx.execute(
                        "INSERT OR REPLACE INTO fee_samples(ts, chain_id, broadcaster_id, token, rate, reliability)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                        params![now, *chain as i64, id, token, rate.to_string(), o.reliability],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    app.notify("market");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fee_topics() {
        assert_eq!(fees_topic_chain("/railgun/v2/0-137-fees/json"), Some(137));
        assert_eq!(fees_topic_chain("/railgun/v2/0-137-transact/json"), None);
    }
}
