//! Background workers: receipts, balances, Chainlink prices, derived profit/USD
//! columns, retention, optional container inspection.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use ruint::aliases::U256;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{
    app::App,
    config::Config,
    db::{self, now_us},
    rpc::{Call, hex_u64},
    units::{WEI_PER_NATIVE, mul_div, parse_uint, to_f64},
};

const SEC: i64 = 1_000_000;

/// Receipt polling backoff after `attempts` misses.
fn backoff_us(attempts: i64) -> i64 {
    let secs = (15i64 << attempts.clamp(0, 6)).min(600);
    secs * SEC
}

pub async fn receipts_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        if let Err(e) = enrich_receipts_once(&app).await {
            tracing::warn!(error = %e, "receipt enrichment failed");
        }
    }
}

pub async fn enrich_receipts_once(app: &App) -> Result<usize> {
    let now = now_us();
    let due: Vec<(String, i64, i64, i64)> = app
        .db
        .read(move |c| {
            let mut st = c.prepare(
                "SELECT tx_hash, chain_id, ts, check_attempts FROM relays
                 WHERE receipt_state='pending' AND next_check_ts <= ?1 ORDER BY ts LIMIT 40",
            )?;
            let rows = st
                .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    if due.is_empty() {
        return Ok(0);
    }
    let mut by_chain: HashMap<i64, Vec<(String, i64, i64)>> = HashMap::new();
    for (h, chain, ts, attempts) in due {
        by_chain.entry(chain).or_default().push((h, ts, attempts));
    }
    let mut updates: Vec<ReceiptUpdate> = Vec::new();
    for (chain, txs) in by_chain {
        let Some(pool) = app.rpcs.get(&(chain as u64)) else {
            for (h, _, attempts) in txs {
                updates.push(ReceiptUpdate::Retry {
                    hash: h,
                    attempts: attempts + 1,
                    next: now + 3600 * SEC,
                    give_up: false,
                });
            }
            continue;
        };
        let calls: Vec<Call> = txs.iter().map(|(h, _, _)| Call::new("eth_getTransactionReceipt", json!([h]))).collect();
        let results = pool.batch(&calls).await;
        for (i, (hash, ts, attempts)) in txs.into_iter().enumerate() {
            let receipt = match &results {
                Ok(r) => r.get(i).and_then(|r| r.as_ref().ok()).cloned(),
                Err(_) => None,
            };
            match receipt.filter(|r| !r.is_null()) {
                Some(r) => updates.push(ReceiptUpdate::Found { hash, receipt: r }),
                None => {
                    let give_up = now - ts > 24 * 3600 * SEC && attempts >= 10;
                    updates.push(ReceiptUpdate::Retry {
                        hash,
                        attempts: attempts + 1,
                        next: now + backoff_us(attempts),
                        give_up,
                    });
                }
            }
        }
    }
    let cfg = app.cfg.clone();
    let n = updates.len();
    let found = updates.iter().any(|u| matches!(u, ReceiptUpdate::Found { .. }));
    app.db
        .write(move |c| {
            let tx = c.transaction()?;
            for u in updates {
                apply_receipt(&tx, &cfg, u)?;
            }
            recompute_relays(&tx, &cfg)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    if found {
        app.notify("relays");
    }
    Ok(n)
}

pub enum ReceiptUpdate {
    Found { hash: String, receipt: Value },
    Retry { hash: String, attempts: i64, next: i64, give_up: bool },
}

pub fn apply_receipt(c: &Connection, cfg: &Config, u: ReceiptUpdate) -> Result<()> {
    match u {
        ReceiptUpdate::Found { hash, receipt } => {
            let chain: i64 = c.query_row("SELECT chain_id FROM relays WHERE tx_hash=?1", [&hash], |r| r.get(0))?;
            let gas_used = receipt.get("gasUsed").and_then(Value::as_str).and_then(parse_uint);
            let price = receipt.get("effectiveGasPrice").and_then(Value::as_str).and_then(parse_uint);
            let cost = gas_used.zip(price).and_then(|(g, p)| g.checked_mul(p));
            let to = receipt.get("to").and_then(Value::as_str).map(str::to_lowercase);
            let route = to.as_ref().map(|to| {
                let is_adapt = cfg.chain(chain as u64).is_some_and(|ch| ch.relay_adapt.iter().any(|a| a == to));
                if is_adapt { "mev" } else { "mempool" }
            });
            c.execute(
                "UPDATE relays SET receipt_state='confirmed', block=?2, status=?3, gas_used=?4,
                    effective_gas_price=?5, cost_wei=?6, tx_to=?7, route=?8,
                    profit_native=NULL, profit_usd=NULL
                 WHERE tx_hash=?1",
                params![
                    hash,
                    receipt.get("blockNumber").and_then(hex_u64).map(|v| v as i64),
                    receipt.get("status").and_then(hex_u64).map(|v| v as i64),
                    gas_used.map(|v| v.to::<u64>() as i64),
                    price.map(|v| v.to_string()),
                    cost.map(|v| v.to_string()),
                    to,
                    route
                ],
            )?;
        }
        ReceiptUpdate::Retry { hash, attempts, next, give_up } => {
            c.execute(
                "UPDATE relays SET check_attempts=?2, next_check_ts=?3,
                    receipt_state=CASE WHEN ?4 THEN 'not_found' ELSE receipt_state END
                 WHERE tx_hash=?1",
                params![hash, attempts, next, give_up],
            )?;
        }
    }
    Ok(())
}

/// Nearest native/USD price within 3 days of `ts` (normally minutes away; the wide
/// window lets replayed older relays get an approximate USD value).
pub fn price_near(c: &Connection, chain: i64, ts: i64) -> Result<Option<f64>> {
    let window = 3 * 24 * 3600 * SEC;
    let before: Option<(i64, f64)> = c
        .query_row(
            "SELECT ts, usd FROM prices WHERE chain_id=?1 AND ts <= ?2 ORDER BY ts DESC LIMIT 1",
            params![chain, ts],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let after: Option<(i64, f64)> = c
        .query_row(
            "SELECT ts, usd FROM prices WHERE chain_id=?1 AND ts >= ?2 ORDER BY ts ASC LIMIT 1",
            params![chain, ts],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let best = match (before, after) {
        (Some(b), Some(a)) => Some(if ts - b.0 <= a.0 - ts { b } else { a }),
        (b, a) => b.or(a),
    };
    Ok(best.filter(|(t, _)| (t - ts).abs() <= window).map(|(_, p)| p))
}

/// Our published rate for chain/token nearest to `ts` (history first, then current).
fn our_rate_near(c: &Connection, chain: i64, token: &str, ts: i64) -> Result<Option<U256>> {
    let r: Option<String> = c
        .query_row(
            "SELECT rate FROM our_fee_history WHERE chain_id=?1 AND token=?2 ORDER BY abs(ts - ?3) LIMIT 1",
            params![chain, token, ts],
            |r| r.get(0),
        )
        .optional()?;
    let r = match r {
        Some(r) => Some(r),
        None => c
            .query_row("SELECT rate FROM our_fees WHERE chain_id=?1 AND token=?2", params![chain, token], |r| r.get(0))
            .optional()?,
    };
    Ok(r.as_deref().and_then(parse_uint))
}

/// (tx_hash, ts, chain, fee_token, fee_amount, fee_native_wei, cost_wei)
type RelayInputs = (String, i64, i64, Option<String>, Option<String>, Option<String>, Option<String>);

/// Fill derived columns (fee value, profit, USD) where inputs became available.
pub fn recompute_relays(c: &Connection, cfg: &Config) -> Result<usize> {
    let rows: Vec<RelayInputs> = {
        let mut st = c.prepare(
            "SELECT tx_hash, ts, chain_id, fee_token, fee_amount, fee_native_wei, cost_wei FROM relays
             WHERE (fee_native IS NULL AND (fee_native_wei IS NOT NULL OR fee_amount IS NOT NULL))
                OR (cost_native IS NULL AND cost_wei IS NOT NULL)
                OR (profit_native IS NULL AND fee_native_wei IS NOT NULL AND cost_wei IS NOT NULL)
                OR (native_usd IS NULL)
             ORDER BY ts DESC LIMIT 500",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut n = 0;
    for (hash, ts, chain, token, amount, fee_wei, cost_wei) in rows {
        let mut fee_wei = fee_wei.as_deref().and_then(parse_uint);
        let mut fee_source: Option<&str> = None;
        if fee_wei.is_none()
            && let (Some(token), Some(amount)) = (token.as_deref(), amount.as_deref().and_then(parse_uint))
            && let Some(rate) = our_rate_near(c, chain, token, ts)?
        {
            // rate = token units per 1e18 wei incl. bonus → market value = amount·1e18·(1+bonus)/rate
            let bonus = cfg.chain(chain as u64).and_then(|ch| ch.fee_bonus).unwrap_or(0.0);
            let scale = U256::from(1_000_000u64);
            let bonus_scaled = U256::from(((1.0 + bonus) * 1e6).round() as u64);
            fee_wei = mul_div(amount, U256::from(WEI_PER_NATIVE) * bonus_scaled, rate * scale);
            fee_source = Some("our_rate");
        }
        let cost = cost_wei.as_deref().and_then(parse_uint);
        let fee_native = fee_wei.map(|v| to_f64(v, 18));
        let cost_native = cost.map(|v| to_f64(v, 18));
        let profit = fee_native.zip(cost_native).map(|(f, c)| f - c);
        let usd = price_near(c, chain, ts)?;
        c.execute(
            "UPDATE relays SET
                fee_native_wei=coalesce(fee_native_wei, ?2),
                fee_native_source=coalesce(fee_native_source, ?3),
                fee_native=?4, cost_native=?5, profit_native=?6, native_usd=?7,
                fee_usd=?4 * ?7, cost_usd=?5 * ?7, profit_usd=?6 * ?7
             WHERE tx_hash=?1",
            params![hash, fee_wei.map(|v| v.to_string()), fee_source, fee_native, cost_native, profit, usd],
        )?;
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------

pub async fn balances_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(app.cfg.health.balance_poll_secs.max(30)));
    loop {
        tick.tick().await;
        for chain in app.cfg.chains.clone() {
            if let Err(e) = poll_balance(&app, chain.id).await {
                tracing::debug!(chain = chain.id, error = %e, "balance poll failed");
            }
        }
        app.notify("health");
    }
}

async fn poll_balance(app: &App, chain: u64) -> Result<()> {
    let Some(pool) = app.rpcs.get(&chain) else { return Ok(()) };
    let wallet = app.cfg.broadcaster.gas_wallet.clone();
    let res = pool
        .batch(&[Call::new("eth_getBalance", json!([wallet, "latest"])), Call::new("eth_gasPrice", json!([]))])
        .await?;
    let balance = res.first().and_then(|r| r.as_ref().ok()).and_then(Value::as_str).and_then(parse_uint);
    let gas_price = res.get(1).and_then(|r| r.as_ref().ok()).and_then(Value::as_str).and_then(parse_uint);
    let now = now_us();
    app.db
        .write(move |c| {
            if let Some(b) = balance {
                let last: Option<(i64, String)> = c
                    .query_row(
                        "SELECT ts, wei FROM balances WHERE chain_id=?1 ORDER BY ts DESC LIMIT 1",
                        [chain as i64],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let changed = last.as_ref().is_none_or(|(_, w)| *w != b.to_string());
                let stale = last.as_ref().is_none_or(|(t, _)| now - t >= 3600 * SEC);
                if changed || stale {
                    c.execute(
                        "INSERT OR REPLACE INTO balances(chain_id, ts, wei, native) VALUES(?1, ?2, ?3, ?4)",
                        params![chain as i64, now, b.to_string(), to_f64(b, 18)],
                    )?;
                }
                db::set_meta(c, &format!("balance_checked_{chain}"), &now.to_string())?;
            }
            if let Some(p) = gas_price {
                c.execute(
                    "INSERT INTO chain_state(chain_id, gas_price_wei, gas_price_ts) VALUES(?1, ?2, ?3)
                     ON CONFLICT(chain_id) DO UPDATE SET gas_price_wei=excluded.gas_price_wei, gas_price_ts=excluded.gas_price_ts",
                    params![chain as i64, p.to_string(), now],
                )?;
            }
            Ok(())
        })
        .await
}

// Chainlink AggregatorV3: latestRoundData() / decimals()
const LATEST_ROUND_DATA: &str = "0xfeaf968c";
const DECIMALS: &str = "0x313ce567";

pub fn decode_latest_round(hex: &str, decimals: u32) -> Option<f64> {
    let b = hex::decode(hex.trim_start_matches("0x")).ok()?;
    if b.len() < 64 {
        return None;
    }
    let answer = U256::from_be_slice(&b[32..64]);
    // int256: reject negatives (top bit set).
    if answer.bit(255) {
        return None;
    }
    let v = to_f64(answer, decimals);
    (v.is_finite() && v > 0.0).then_some(v)
}

pub async fn prices_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(app.cfg.health.price_poll_secs.max(60)));
    let mut decimals: HashMap<u64, u32> = HashMap::new();
    loop {
        tick.tick().await;
        for chain in app.cfg.chains.clone() {
            let price = match (&chain.native_usd_feed, app.rpcs.get(&chain.id)) {
                (Some(feed), Some(pool)) => {
                    let mut calls =
                        vec![Call::new("eth_call", json!([{"to": feed, "data": LATEST_ROUND_DATA}, "latest"]))];
                    if !decimals.contains_key(&chain.id) {
                        calls.push(Call::new("eth_call", json!([{"to": feed, "data": DECIMALS}, "latest"])));
                    }
                    match pool.batch(&calls).await {
                        Ok(res) => {
                            if let Some(d) =
                                res.get(1).and_then(|r| r.as_ref().ok()).and_then(Value::as_str).and_then(parse_uint)
                            {
                                decimals.insert(chain.id, d.to::<u32>());
                            }
                            let d = decimals.get(&chain.id).copied().unwrap_or(8);
                            res.first()
                                .and_then(|r| r.as_ref().ok())
                                .and_then(Value::as_str)
                                .and_then(|h| decode_latest_round(h, d))
                                .map(|p| (p, "chainlink"))
                        }
                        Err(e) => {
                            tracing::debug!(chain = chain.id, error = %e, "chainlink poll failed");
                            None
                        }
                    }
                }
                _ => None,
            };
            let cfg = app.cfg.clone();
            let id = chain.id;
            let res = app
                .db
                .write(move |c| {
                    let price = match price {
                        Some(p) => Some(p),
                        None => derived_native_usd(c, &cfg, id)?.map(|p| (p, "our_fees")),
                    };
                    if let Some((usd, source)) = price {
                        c.execute(
                            "INSERT OR REPLACE INTO prices(chain_id, ts, usd, source) VALUES(?1, ?2, ?3, ?4)",
                            params![id as i64, now_us(), usd, source],
                        )?;
                    }
                    recompute_relays(c, &cfg)?;
                    Ok(())
                })
                .await;
            if let Err(e) = res {
                tracing::warn!(chain = chain.id, error = %e, "price update failed");
            }
        }
        app.notify("relays");
    }
}

/// Native/USD implied by our own published stablecoin rate: rate/10^dec/(1+bonus).
pub fn derived_native_usd(c: &Connection, cfg: &Config, chain: u64) -> Result<Option<f64>> {
    let Some(ch) = cfg.chain(chain) else { return Ok(None) };
    let bonus = ch.fee_bonus.unwrap_or(0.0);
    let mut vals = Vec::new();
    for t in ch.tokens.iter().filter(|t| t.stable) {
        let rate: Option<String> = c
            .query_row(
                "SELECT rate FROM our_fees WHERE chain_id=?1 AND token=?2 AND ts > ?3",
                params![chain as i64, t.address, now_us() - 3600 * SEC],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(r) = rate.as_deref().and_then(parse_uint) {
            vals.push(to_f64(r, t.decimals) / (1.0 + bonus));
        }
    }
    if vals.is_empty() {
        return Ok(None);
    }
    vals.sort_by(|a, b| a.total_cmp(b));
    Ok(Some(vals[vals.len() / 2]))
}

// ---------------------------------------------------------------------------

pub async fn maintenance_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let storage = app.cfg.storage.clone();
        match app.db.write(move |c| db::prune(c, &storage)).await {
            Ok(n) if n > 0 => tracing::info!(rows = n, "pruned old rows"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "prune failed"),
        }
    }
}

pub async fn container_loop(app: Arc<App>) {
    let argv = app.cfg.health.container_inspect.clone();
    if argv.is_empty() {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        let out = tokio::process::Command::new(&argv[0]).args(&argv[1..]).kill_on_drop(true).output().await;
        let summary = match out {
            Ok(o) if o.status.success() => {
                parse_inspect(&o.stdout).unwrap_or_else(|| json!({"error": "unparseable docker inspect output"}))
            }
            Ok(o) => json!({"error": format!("exit {}", o.status)}),
            Err(e) => json!({"error": e.to_string()}),
        };
        let mut summary = summary;
        summary["checked"] = json!(now_us());
        let s = summary.to_string();
        let _ = app.db.write(move |c| db::set_meta(c, "container", &s)).await;
        app.notify("health");
    }
}

pub fn parse_inspect(stdout: &[u8]) -> Option<Value> {
    let v: Value = serde_json::from_slice(stdout).ok()?;
    let c = v.as_array()?.first()?;
    let state = c.get("State")?;
    Some(json!({
        "status": state.get("Status"),
        "health": state.pointer("/Health/Status"),
        "started_at": state.get("StartedAt"),
        "restart_count": c.get("RestartCount"),
        "image": c.pointer("/Config/Image"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_chainlink_answer() {
        // roundId=1, answer=250_000_000_000 (2500.00000000 with 8 decimals)
        let mut words = vec![U256::from(1u64), U256::from(250_000_000_000u64)];
        words.extend([U256::ZERO; 3]);
        let hex: String = words.iter().map(|w| hex::encode(w.to_be_bytes::<32>())).collect();
        assert_eq!(decode_latest_round(&format!("0x{hex}"), 8), Some(2500.0));
    }

    #[test]
    fn parses_docker_inspect() {
        let s = br#"[{"State":{"Status":"running","StartedAt":"2026-10-17T20:00:00Z","Health":{"Status":"healthy"}},"RestartCount":0,"Config":{"Image":"railgun-broadcaster:e5b36fc-ppoiprev"}}]"#;
        let v = parse_inspect(s).unwrap();
        assert_eq!(v["health"], "healthy");
        assert_eq!(v["status"], "running");
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_us(0), 15 * SEC);
        assert_eq!(backoff_us(2), 60 * SEC);
        assert_eq!(backoff_us(10), 600 * SEC);
    }
}
