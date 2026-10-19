//! Read-side SQL for the views.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{db, units};

pub const DAY_US: i64 = 86_400_000_000;
pub const HOUR_US: i64 = 3_600_000_000;

#[derive(Debug, Default, Clone)]
pub struct ChainHealth {
    pub chain: i64,
    pub last_fee_publish: Option<i64>,
    pub last_start: Option<i64>,
    pub last_event: Option<i64>,
    pub available_wallets: Option<i64>,
    pub version: Option<String>,
    pub gas_price_wei: Option<f64>,
    pub last_est_cost: Option<f64>,
    pub balance: Option<f64>,
    pub balance_ts: Option<i64>,
    pub price_usd: Option<f64>,
    pub price_source: Option<String>,
    /// (average actual cost in native, number of relays it's based on)
    pub avg_cost: Option<(f64, i64)>,
    pub counts_1h: HashMap<String, i64>,
    pub counts_24h: HashMap<String, i64>,
    pub relays_7d: i64,
    pub profit_7d_native: Option<f64>,
    pub profit_7d_usd: Option<f64>,
    pub problems_7d: i64,
}

pub fn chain_health(c: &Connection, chain: i64, now: i64) -> Result<ChainHealth> {
    let mut h = ChainHealth { chain, ..Default::default() };
    if let Some(r) = c
        .query_row(
            "SELECT last_fee_publish, last_start, last_event, available_wallets, version, gas_price_wei, last_est_cost_wei
             FROM chain_state WHERE chain_id=?1",
            [chain],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?
    {
        h.last_fee_publish = r.0;
        h.last_start = r.1;
        h.last_event = r.2;
        h.available_wallets = r.3;
        h.version = r.4;
        h.gas_price_wei = r.5.as_deref().and_then(|v| units::dec_to_f64(v, 0));
        h.last_est_cost = r.6.as_deref().and_then(|v| units::dec_to_f64(v, 18));
    }
    if let Some((ts, native)) = c
        .query_row("SELECT ts, native FROM balances WHERE chain_id=?1 ORDER BY ts DESC LIMIT 1", [chain], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })
        .optional()?
    {
        h.balance = Some(native);
        h.balance_ts = Some(ts);
    }
    if let Some((usd, src)) = c
        .query_row("SELECT usd, source FROM prices WHERE chain_id=?1 ORDER BY ts DESC LIMIT 1", [chain], |r| {
            Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?))
        })
        .optional()?
    {
        h.price_usd = Some(usd);
        h.price_source = Some(src);
    }
    let (avg, n): (Option<f64>, i64) = c.query_row(
        "SELECT avg(cost_native), count(*) FROM (
            SELECT cost_native FROM relays WHERE chain_id=?1 AND cost_native IS NOT NULL ORDER BY ts DESC LIMIT 20)",
        [chain],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    h.avg_cost = avg.filter(|_| n > 0).map(|a| (a, n));
    for (window, map) in [(HOUR_US, &mut h.counts_1h), (DAY_US, &mut h.counts_24h)] {
        let mut st = c.prepare_cached(
            "SELECT kind, sum(count) FROM hourly_counters WHERE chain_id=?1 AND hour >= ?2 GROUP BY kind",
        )?;
        let rows = st.query_map(params![chain, (now - window).div_euclid(HOUR_US)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for r in rows {
            let (k, v) = r?;
            map.insert(k, v);
        }
    }
    let (n, p, pu): (i64, Option<f64>, Option<f64>) = c.query_row(
        "SELECT count(*), sum(profit_native), sum(profit_usd) FROM relays WHERE chain_id=?1 AND ts >= ?2",
        params![chain, now - 7 * DAY_US],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    h.relays_7d = n;
    h.profit_7d_native = p;
    h.profit_7d_usd = pu;
    h.problems_7d = c.query_row(
        "SELECT count(*) FROM requests WHERE chain_id=?1 AND ts >= ?2 AND outcome IN ('rejected','failed','unknown')",
        params![chain, now - 7 * DAY_US],
        |r| r.get(0),
    )?;
    Ok(h)
}

#[derive(Debug, Default)]
pub struct Totals {
    pub relays_24h: i64,
    pub relays_7d: i64,
    pub profit_7d_usd: Option<f64>,
    pub profit_7d_unpriced: i64,
    pub problems_24h: i64,
    pub unparsed_24h: i64,
    pub other_24h: i64,
    pub last_log_ts: Option<i64>,
    pub last_ingest_wall: Option<i64>,
    pub events_total: i64,
    pub container: Option<serde_json::Value>,
    pub last_restart: Option<(i64, String)>,
}

pub fn totals(c: &Connection, now: i64) -> Result<Totals> {
    let mut t = Totals::default();
    (t.relays_24h, t.relays_7d, t.profit_7d_usd, t.profit_7d_unpriced) = c.query_row(
        "SELECT sum(ts >= ?1), count(*), sum(profit_usd), sum(profit_usd IS NULL) FROM relays WHERE ts >= ?2",
        params![now - DAY_US, now - 7 * DAY_US],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get(1)?,
                r.get(2)?,
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            ))
        },
    )?;
    t.problems_24h = c.query_row(
        "SELECT count(*) FROM requests WHERE ts >= ?1 AND outcome IN ('rejected','failed','unknown')",
        [now - DAY_US],
        |r| r.get(0),
    )?;
    let since_hour = (now - DAY_US).div_euclid(HOUR_US);
    t.unparsed_24h = c.query_row(
        "SELECT coalesce(sum(count),0) FROM hourly_counters WHERE kind='unparsed' AND hour >= ?1",
        [since_hour],
        |r| r.get(0),
    )?;
    t.other_24h = c.query_row(
        "SELECT coalesce(sum(count),0) FROM hourly_counters WHERE kind='other' AND hour >= ?1",
        [since_hour],
        |r| r.get(0),
    )?;
    t.last_log_ts = db::get_meta(c, "last_log_ts")?.and_then(|v| v.parse().ok());
    t.last_ingest_wall = db::get_meta(c, "last_ingest_wall")?.and_then(|v| v.parse().ok());
    t.events_total = c.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
    t.container = db::get_meta(c, "container")?.and_then(|v| serde_json::from_str(&v).ok());
    t.last_restart = c
        .query_row("SELECT ts, chains FROM restarts ORDER BY ts DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(t)
}

#[derive(Debug, Clone)]
pub struct RelayRow {
    pub tx_hash: String,
    pub ts: i64,
    pub chain: i64,
    pub fee_token: Option<String>,
    pub fee_amount: Option<String>,
    pub fee_native: Option<f64>,
    pub fee_native_source: Option<String>,
    pub fee_usd: Option<f64>,
    pub est_cost: Option<f64>,
    pub cost_native: Option<f64>,
    pub cost_usd: Option<f64>,
    pub profit_native: Option<f64>,
    pub profit_usd: Option<f64>,
    pub route: Option<String>,
    pub receipt_state: String,
    pub status: Option<i64>,
    pub assurance: Option<String>,
    pub assurance_detail: Option<String>,
    pub source: String,
    pub request_id: Option<i64>,
}

impl RelayRow {
    pub fn margin(&self) -> Option<f64> {
        self.profit_native.zip(self.cost_native).filter(|(_, c)| *c > 0.0).map(|(p, c)| p / c)
    }
}

pub fn relays(c: &Connection, chain: Option<i64>, since: i64, limit: i64, offset: i64) -> Result<(Vec<RelayRow>, i64)> {
    let total: i64 = c.query_row(
        "SELECT count(*) FROM relays WHERE ts >= ?1 AND (?2 IS NULL OR chain_id = ?2)",
        params![since, chain],
        |r| r.get(0),
    )?;
    let mut st = c.prepare(
        "SELECT tx_hash, ts, chain_id, fee_token, fee_amount, fee_native, fee_native_source, fee_usd, est_cost_wei,
                cost_native, cost_usd, profit_native, profit_usd, route, receipt_state, status, assurance,
                assurance_detail, source, request_id
         FROM relays WHERE ts >= ?1 AND (?2 IS NULL OR chain_id = ?2)
         ORDER BY ts DESC LIMIT ?3 OFFSET ?4",
    )?;
    let rows = st
        .query_map(params![since, chain, limit, offset], |r| {
            Ok(RelayRow {
                tx_hash: r.get(0)?,
                ts: r.get(1)?,
                chain: r.get(2)?,
                fee_token: r.get(3)?,
                fee_amount: r.get(4)?,
                fee_native: r.get(5)?,
                fee_native_source: r.get(6)?,
                fee_usd: r.get(7)?,
                est_cost: r.get::<_, Option<String>>(8)?.as_deref().and_then(|v| units::dec_to_f64(v, 18)),
                cost_native: r.get(9)?,
                cost_usd: r.get(10)?,
                profit_native: r.get(11)?,
                profit_usd: r.get(12)?,
                route: r.get(13)?,
                receipt_state: r.get(14)?,
                status: r.get(15)?,
                assurance: r.get(16)?,
                assurance_detail: r.get(17)?,
                source: r.get(18)?,
                request_id: r.get(19)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok((rows, total))
}

#[derive(Debug, Clone)]
pub struct RequestRow {
    pub id: i64,
    pub ts: i64,
    pub chain: Option<i64>,
    pub fee_token: Option<String>,
    pub fee_amount: Option<String>,
    pub stage: i64,
    pub outcome: String,
    pub reason: Option<String>,
    pub detail: Option<String>,
    pub tx_hash: Option<String>,
    pub est_cost: Option<f64>,
    pub refund: Option<f64>,
    pub rpc_clients: Option<String>,
}

pub fn outcome_filter(f: &str) -> &'static [&'static str] {
    match f {
        "all" => &["pending", "submitted", "rejected", "failed", "unknown"],
        "submitted" => &["submitted"],
        "pending" => &["pending"],
        _ => &["rejected", "failed", "unknown"],
    }
}

pub fn requests(
    c: &Connection,
    chain: Option<i64>,
    outcomes: &[&str],
    since: i64,
    limit: i64,
    offset: i64,
) -> Result<(Vec<RequestRow>, i64)> {
    let ph = outcomes.iter().map(|o| format!("'{o}'")).collect::<Vec<_>>().join(",");
    let total: i64 = c.query_row(
        &format!(
            "SELECT count(*) FROM requests WHERE ts >= ?1 AND (?2 IS NULL OR chain_id = ?2) AND outcome IN ({ph})"
        ),
        params![since, chain],
        |r| r.get(0),
    )?;
    let mut st = c.prepare(&format!(
        "SELECT id, ts, chain_id, fee_token, fee_amount, stage, outcome, reason, detail, tx_hash, est_cost_wei, refund_wei, rpc_clients
         FROM requests WHERE ts >= ?1 AND (?2 IS NULL OR chain_id = ?2) AND outcome IN ({ph})
         ORDER BY ts DESC, id DESC LIMIT ?3 OFFSET ?4"
    ))?;
    let rows = st.query_map(params![since, chain, limit, offset], request_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok((rows, total))
}

fn request_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        chain: r.get(2)?,
        fee_token: r.get(3)?,
        fee_amount: r.get(4)?,
        stage: r.get(5)?,
        outcome: r.get(6)?,
        reason: r.get(7)?,
        detail: r.get(8)?,
        tx_hash: r.get(9)?,
        est_cost: r.get::<_, Option<String>>(10)?.as_deref().and_then(|v| units::dec_to_f64(v, 18)),
        refund: r.get::<_, Option<String>>(11)?.as_deref().and_then(|v| units::dec_to_f64(v, 18)),
        rpc_clients: r.get(12)?,
    })
}

pub fn request(c: &Connection, id: i64) -> Result<Option<RequestRow>> {
    Ok(c.query_row(
        "SELECT id, ts, chain_id, fee_token, fee_amount, stage, outcome, reason, detail, tx_hash, est_cost_wei, refund_wei, rpc_clients
         FROM requests WHERE id=?1",
        [id],
        request_row,
    )
    .optional()?)
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub ts: i64,
    pub level: String,
    pub chain: Option<i64>,
    pub kind: String,
    pub message: String,
    pub fields: Option<String>,
    pub request_id: Option<i64>,
}

pub fn events(
    c: &Connection,
    kind: Option<&str>,
    chain: Option<i64>,
    request: Option<i64>,
    limit: i64,
) -> Result<Vec<EventRow>> {
    let mut st = c.prepare(
        "SELECT ts, level, chain_id, kind, message, fields, request_id FROM events
         WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR chain_id = ?2) AND (?3 IS NULL OR request_id = ?3)
         ORDER BY ts DESC, id DESC LIMIT ?4",
    )?;
    let rows = st
        .query_map(params![kind, chain, request, limit], |r| {
            Ok(EventRow {
                ts: r.get(0)?,
                level: r.get(1)?,
                chain: r.get(2)?,
                kind: r.get(3)?,
                message: r.get(4)?,
                fields: r.get(5)?,
                request_id: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn kind_counts(c: &Connection, since: i64) -> Result<Vec<(String, i64)>> {
    let mut st =
        c.prepare("SELECT kind, sum(count) AS n FROM hourly_counters WHERE hour >= ?1 GROUP BY kind ORDER BY n DESC")?;
    let rows =
        st.query_map([since.div_euclid(HOUR_US)], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone)]
pub struct ReasonGroup {
    pub reason: String,
    pub chain: Option<i64>,
    pub outcome: String,
    pub count: i64,
    pub first: i64,
    pub last: i64,
}

pub fn reason_groups(c: &Connection, since: i64, chain: Option<i64>) -> Result<Vec<ReasonGroup>> {
    let mut st = c.prepare(
        "SELECT coalesce(reason, '(none)'), chain_id, outcome, count(*), min(ts), max(ts) FROM requests
         WHERE ts >= ?1 AND (?2 IS NULL OR chain_id = ?2) AND outcome IN ('rejected','failed','unknown')
         GROUP BY 1, 2, 3 ORDER BY 4 DESC",
    )?;
    let rows = st
        .query_map(params![since, chain], |r| {
            Ok(ReasonGroup {
                reason: r.get(0)?,
                chain: r.get(1)?,
                outcome: r.get(2)?,
                count: r.get(3)?,
                first: r.get(4)?,
                last: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// (day start µs, reason, count) for problem requests.
pub fn reason_by_day(c: &Connection, since: i64, chain: Option<i64>) -> Result<Vec<(i64, String, i64)>> {
    let mut st = c.prepare(
        "SELECT (ts / 86400000000) * 86400000000 AS d, coalesce(reason, '(none)'), count(*) FROM requests
         WHERE ts >= ?1 AND (?2 IS NULL OR chain_id = ?2) AND outcome IN ('rejected','failed','unknown')
         GROUP BY 1, 2 ORDER BY 1 DESC, 3 DESC",
    )?;
    let rows = st
        .query_map(params![since, chain], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Request outcome counts per chain (for the funnel line on the requests page).
pub fn outcome_counts(c: &Connection, since: i64, chain: Option<i64>) -> Result<HashMap<String, i64>> {
    let mut st = c.prepare(
        "SELECT outcome, count(*) FROM requests WHERE ts >= ?1 AND (?2 IS NULL OR chain_id = ?2) GROUP BY 1",
    )?;
    let rows = st.query_map(params![since, chain], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let mut out = HashMap::new();
    for r in rows {
        let (k, v) = r?;
        out.insert(k, v);
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct Agg {
    pub relays: i64,
    pub fee_native: f64,
    pub cost_native: f64,
    pub profit_native: f64,
    pub profit_usd: f64,
    pub fee_usd: f64,
    pub cost_usd: f64,
    /// relays without a known profit (fee or cost missing)
    pub unknown: i64,
    pub unpriced: i64,
}

/// Per (day, chain) aggregates.
pub fn daily(c: &Connection, since: i64) -> Result<Vec<(i64, i64, Agg)>> {
    let mut st = c.prepare(
        "SELECT ts / 86400000000 AS d, chain_id, count(*),
                coalesce(sum(fee_native),0), coalesce(sum(cost_native),0), coalesce(sum(profit_native),0),
                coalesce(sum(profit_usd),0), coalesce(sum(fee_usd),0), coalesce(sum(cost_usd),0),
                sum(profit_native IS NULL), sum(profit_usd IS NULL)
         FROM relays WHERE ts >= ?1 GROUP BY 1, 2 ORDER BY 1",
    )?;
    let rows = st
        .query_map([since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                Agg {
                    relays: r.get(2)?,
                    fee_native: r.get(3)?,
                    cost_native: r.get(4)?,
                    profit_native: r.get(5)?,
                    profit_usd: r.get(6)?,
                    fee_usd: r.get(7)?,
                    cost_usd: r.get(8)?,
                    unknown: r.get(9)?,
                    unpriced: r.get(10)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// (chain, fee token, aggregates, summed fee amount in base units)
pub type TokenAgg = (i64, Option<String>, Agg, f64);

pub fn by_token(c: &Connection, since: i64) -> Result<Vec<TokenAgg>> {
    let mut st = c.prepare(
        "SELECT chain_id, fee_token, count(*),
                coalesce(sum(fee_native),0), coalesce(sum(cost_native),0), coalesce(sum(profit_native),0),
                coalesce(sum(profit_usd),0), coalesce(sum(fee_usd),0), coalesce(sum(cost_usd),0),
                sum(profit_native IS NULL), sum(profit_usd IS NULL), group_concat(fee_amount, ',')
         FROM relays WHERE ts >= ?1 GROUP BY 1, 2 ORDER BY 7 DESC",
    )?;
    let rows = st
        .query_map([since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                Agg {
                    relays: r.get(2)?,
                    fee_native: r.get(3)?,
                    cost_native: r.get(4)?,
                    profit_native: r.get(5)?,
                    profit_usd: r.get(6)?,
                    fee_usd: r.get(7)?,
                    cost_usd: r.get(8)?,
                    unknown: r.get(9)?,
                    unpriced: r.get(10)?,
                },
                r.get::<_, Option<String>>(11)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    // Sum raw token amounts exactly; decimals are applied by the view.
    Ok(rows
        .into_iter()
        .map(|(chain, tok, agg, amounts)| {
            let sum = amounts
                .unwrap_or_default()
                .split(',')
                .filter_map(units::parse_uint)
                .fold(ruint::aliases::U256::ZERO, |a, b| a.saturating_add(b));
            (chain, tok, agg, crate::market::u256_f64(sum))
        })
        .collect())
}

/// Share of samples in the window where wallets would have picked us.
pub fn pick_share(c: &Connection, chain: i64, token: &str, since: i64) -> Result<Option<(f64, i64)>> {
    let (inpool, n): (Option<i64>, i64) = c.query_row(
        "SELECT sum(picked), count(picked) FROM market_snapshots WHERE chain_id=?1 AND token=?2 AND ts >= ?3",
        params![chain, token, since],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((n > 0).then(|| (inpool.unwrap_or(0) as f64 / n as f64, n)))
}
