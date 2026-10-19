//! Log line → events → correlated requests/relays.
//!
//! Correlation state lives in the `requests` table (outcome = 'pending'), so a
//! dashboard restart in the middle of a request loses nothing. Facts the
//! correlation relies on (broadcaster @ e5b36fc):
//! - Waku transact messages are handled one at a time across all chains, and the
//!   span-less `failed to handle transact request` line follows the request's
//!   `parsed transact calldata` line.
//! - Each chain has a single sequential tx worker: `converting value to gas token` →
//!   `estimated gas` → `tx broadcasted`* → `submitted tx` (or a terminal failure).

use std::collections::BTreeSet;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use crate::{
    parse::{self, Kind, Line},
    redact,
    units::{parse_decimal_units, parse_uint},
};

/// Pending requests without an outcome after this long (log time) become `unknown`.
pub const PENDING_TIMEOUT_US: i64 = 600 * 1_000_000;
/// A span-less rejection is attributed to a parsed request at most this old.
pub const REJECT_WINDOW_US: i64 = 120 * 1_000_000;
/// Our-fee history is sampled at most this often per chain/token.
const OUR_FEE_HISTORY_US: i64 = 900 * 1_000_000;

#[derive(Debug, Default, Clone)]
pub struct BatchStats {
    pub new_events: usize,
    pub duplicates: usize,
    pub unparsed: usize,
    pub last_ts: Option<i64>,
    pub topics: BTreeSet<&'static str>,
}

pub fn fnv1a(bytes: &[u8]) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h as i64
}

pub fn outcome_for_reason(reason: &str) -> &'static str {
    const REJECT_PREFIXES: &[&str] = &[
        "Poi/",
        "Decrypt/",
        "Parse/",
        "Tx/GasCostTooHigh",
        "Tx/InvalidTx7702Request",
        "Tx/Tx7702Unsupported",
        "Tx/GasPriceRejected",
    ];
    if REJECT_PREFIXES.iter().any(|p| reason.starts_with(p)) || reason == "Poi" { "rejected" } else { "failed" }
}

/// Ingest a batch of raw lines in one transaction.
pub fn ingest_lines(conn: &mut Connection, lines: &[String]) -> Result<BatchStats> {
    let mut stats = BatchStats::default();
    let tx = conn.transaction()?;
    let mut last_ts: Option<i64> = None;
    for raw in lines {
        ingest_one(&tx, raw, &mut last_ts, &mut stats)?;
    }
    if let Some(ts) = stats.last_ts {
        let n = tx.execute(
            "UPDATE requests SET outcome='unknown', reason='Timeout/NoOutcome', updated_ts=?1
             WHERE outcome='pending' AND ts < ?1 - ?2",
            params![ts, PENDING_TIMEOUT_US],
        )?;
        if n > 0 {
            stats.topics.insert("requests");
        }
        let prev: Option<i64> = crate::db::get_meta(&tx, "last_log_ts")?.and_then(|v| v.parse().ok());
        if prev.is_none_or(|p| ts > p) {
            crate::db::set_meta(&tx, "last_log_ts", &ts.to_string())?;
        }
    }
    if stats.new_events > 0 {
        crate::db::set_meta(&tx, "last_ingest_wall", &crate::db::now_us().to_string())?;
    }
    tx.commit()?;
    Ok(stats)
}

fn ingest_one(tx: &Transaction<'_>, raw: &str, last_ts: &mut Option<i64>, stats: &mut BatchStats) -> Result<()> {
    let stripped = parse::strip_ansi(raw);
    let s = stripped.trim_end_matches(['\r', '\n']);
    if s.trim().is_empty() {
        return Ok(());
    }
    let hash = fnv1a(s.as_bytes());

    let Some(line) = parse::parse_line(s) else {
        // Not a tracing line (panic output, eyre report continuation, …).
        let ts = last_ts.unwrap_or_else(crate::db::now_us);
        let msg: String = s.chars().take(2000).collect();
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO events(hash, ts, level, chain_id, kind, message, fields, parser_version)
             VALUES(?1, ?2, 'UNKNOWN', NULL, 'unparsed', ?3, NULL, ?4)",
            params![hash, ts, redact::redact_text(&msg), parse::PARSER_VERSION],
        )?;
        if inserted == 0 {
            stats.duplicates += 1;
        } else {
            stats.new_events += 1;
            stats.unparsed += 1;
            bump_counter(tx, None, "unparsed", ts)?;
            stats.topics.insert("health");
        }
        return Ok(());
    };

    *last_ts = Some(line.ts);
    let (kind, reason) = parse::classify(&line);
    let chain = line.chain_id();

    let mut fields = Map::new();
    if !line.spans.is_empty() {
        let spans = line
            .spans
            .iter()
            .map(|s| {
                if s.fields.is_empty() {
                    s.name.clone()
                } else {
                    let f: Vec<String> = s.fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    format!("{}{{{}}}", s.name, f.join(" "))
                }
            })
            .collect::<Vec<_>>()
            .join(":");
        fields.insert("_spans".into(), Value::String(redact::redact_text(&spans)));
    }
    for (k, v) in &line.fields {
        if kind == Kind::OurFees && k == "payload" {
            continue; // stored structurally in our_fees
        }
        let v: String = v.chars().take(4000).collect();
        fields.insert(k.clone(), Value::String(redact::redact_field(k, &v)));
    }
    if let Some(r) = &reason {
        fields.insert("_reason".into(), Value::String(r.clone()));
    }
    let fields_json = if fields.is_empty() { None } else { Some(Value::Object(fields).to_string()) };

    let inserted = tx.execute(
        "INSERT OR IGNORE INTO events(hash, ts, level, chain_id, kind, message, fields, parser_version)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            hash,
            line.ts,
            line.level.as_str(),
            chain.map(|c| c as i64),
            kind.as_str(),
            redact::redact_text(&line.message),
            fields_json,
            parse::PARSER_VERSION
        ],
    )?;
    if inserted == 0 {
        stats.duplicates += 1;
        return Ok(());
    }
    let event_id = tx.last_insert_rowid();
    stats.new_events += 1;
    stats.last_ts = Some(stats.last_ts.map_or(line.ts, |t: i64| t.max(line.ts)));
    stats.topics.insert("health");
    bump_counter(tx, chain, kind.as_str(), line.ts)?;
    if let Some(c) = chain {
        tx.execute(
            "INSERT INTO chain_state(chain_id, last_event) VALUES(?1, ?2)
             ON CONFLICT(chain_id) DO UPDATE SET last_event=max(coalesce(last_event, 0), excluded.last_event)",
            params![c as i64, line.ts],
        )?;
    }

    let request_id = apply(tx, &line, kind, reason.as_deref(), chain, stats)?;
    if let Some(id) = request_id {
        tx.execute("UPDATE events SET request_id=?1 WHERE id=?2", params![id, event_id])?;
    }
    Ok(())
}

fn bump_counter(tx: &Transaction<'_>, chain: Option<u64>, kind: &str, ts: i64) -> Result<()> {
    let hour = ts.div_euclid(3_600_000_000);
    tx.execute(
        "INSERT INTO hourly_counters(chain_id, kind, hour, count) VALUES(?1, ?2, ?3, 1)
         ON CONFLICT(chain_id, kind, hour) DO UPDATE SET count=count+1",
        params![chain.map_or(0, |c| c as i64), kind, hour],
    )?;
    Ok(())
}

fn insert_request(
    tx: &Transaction<'_>,
    l: &Line,
    chain: Option<u64>,
    stage: i64,
    outcome: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let detail = l.field("error").map(redact::redact_text);
    tx.execute(
        "INSERT INTO requests(ts, chain_id, fee_token, fee_amount, railgun_txid, stage, outcome, reason, detail, updated_ts)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?1)",
        params![
            l.ts,
            chain.map(|c| c as i64),
            l.field("fee_token").or(l.field("token")).map(str::to_lowercase),
            l.field("fee_amount").or(l.field("amount")),
            l.field("railgun_txid"),
            stage,
            outcome,
            reason,
            detail,
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Oldest pending request on `chain` below `max_stage`. Requests that already reached
/// `prefer_stage` (and match token/amount, when known) win, so a stale request whose
/// outcome line was missed can't steal the next request's tx-stage lines.
fn oldest_pending(
    tx: &Transaction<'_>,
    chain: u64,
    max_stage: i64,
    prefer_stage: i64,
    ts: i64,
    token_amount: Option<(&str, &str)>,
) -> Result<Option<i64>> {
    let (tok, amt) = token_amount.map_or((None, None), |(t, a)| (Some(t.to_lowercase()), Some(a.to_string())));
    Ok(tx
        .query_row(
            "SELECT id FROM requests
             WHERE outcome='pending' AND chain_id=?1 AND stage < ?2 AND ts >= ?3 - ?4 AND ts <= ?3 + 5000000
             ORDER BY (fee_token IS ?5 AND fee_amount IS ?6) DESC, (stage >= ?7) DESC, ts ASC, id ASC
             LIMIT 1",
            params![chain as i64, max_stage, ts, PENDING_TIMEOUT_US, tok, amt, prefer_stage],
            |r| r.get(0),
        )
        .optional()?)
}

fn conclude(tx: &Transaction<'_>, id: i64, l: &Line, reason: &str) -> Result<()> {
    let detail = l.field("error").map(redact::redact_text);
    tx.execute(
        "UPDATE requests SET outcome=?2, reason=?3, detail=coalesce(?4, detail), updated_ts=?5 WHERE id=?1",
        params![id, outcome_for_reason(reason), reason, detail, l.ts],
    )?;
    Ok(())
}

fn apply(
    tx: &Transaction<'_>,
    l: &Line,
    kind: Kind,
    reason: Option<&str>,
    chain: Option<u64>,
    stats: &mut BatchStats,
) -> Result<Option<i64>> {
    match kind {
        Kind::RequestParsed => {
            stats.topics.insert("requests");
            return insert_request(tx, l, chain, 1, "pending", None).map(Some);
        }
        Kind::RequestFailed => {
            stats.topics.insert("requests");
            let reason = reason.unwrap_or("Unknown");
            // Decrypt/Parse failures happen before `parsed transact calldata`.
            let attributable = !(reason.starts_with("Decrypt") || reason.starts_with("Parse"));
            let candidate: Option<i64> = if attributable {
                tx.query_row(
                    "SELECT id, stage FROM requests
                     WHERE outcome='pending' AND ts >= ?1 - ?2 AND ts <= ?1
                     ORDER BY ts DESC, id DESC LIMIT 1",
                    params![l.ts, REJECT_WINDOW_US],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()?
                .filter(|&(_, stage)| stage == 1)
                .map(|(id, _)| id)
            } else {
                None
            };
            return match candidate {
                Some(id) => conclude(tx, id, l, reason).map(|()| Some(id)),
                None => insert_request(tx, l, None, 1, outcome_for_reason(reason), Some(reason)).map(Some),
            };
        }
        Kind::TransactParseFailed => {
            stats.topics.insert("requests");
            let reason = reason.unwrap_or("Parse/Unknown");
            return insert_request(tx, l, chain, 1, outcome_for_reason(reason), Some(reason)).map(Some);
        }
        _ => {}
    }

    let Some(chain) = chain else {
        return Ok(None);
    };

    match kind {
        Kind::ConvertingValue => {
            stats.topics.insert("requests");
            let ta = l.field("token").zip(l.field("amount"));
            let id = match oldest_pending(tx, chain, 2, 1, l.ts, ta)? {
                Some(id) => id,
                None => insert_request(tx, l, Some(chain), 2, "pending", None)?,
            };
            tx.execute(
                "UPDATE requests SET stage=2, updated_ts=?2,
                     fee_token=coalesce(fee_token, ?3), fee_amount=coalesce(fee_amount, ?4)
                 WHERE id=?1",
                params![id, l.ts, l.field("token").map(str::to_lowercase), l.field("amount")],
            )?;
            Ok(Some(id))
        }
        Kind::GasEstimated => {
            stats.topics.insert("requests");
            let id = match oldest_pending(tx, chain, 3, 2, l.ts, None)? {
                Some(id) => id,
                None => insert_request(tx, l, Some(chain), 3, "pending", None)?,
            };
            let cost = l.field("cost").and_then(|v| parse_decimal_units(v, 18)).map(|v| v.to_string());
            let refund = l.field("refund").and_then(|v| parse_decimal_units(v, 18)).map(|v| v.to_string());
            tx.execute(
                "UPDATE requests SET stage=3, updated_ts=?2, gas=?3, gas_price=?4, est_cost_wei=?5, refund_wei=?6 WHERE id=?1",
                params![
                    id,
                    l.ts,
                    l.field("gas").and_then(|v| v.parse::<i64>().ok()),
                    l.field("gas_price").and_then(parse_uint).map(|v| v.to_string()),
                    cost,
                    refund
                ],
            )?;
            if let Some(cost) = &cost {
                tx.execute(
                    "INSERT INTO chain_state(chain_id, last_est_cost_wei, last_est_ts) VALUES(?1, ?2, ?3)
                     ON CONFLICT(chain_id) DO UPDATE SET last_est_cost_wei=excluded.last_est_cost_wei, last_est_ts=excluded.last_est_ts",
                    params![chain as i64, cost, l.ts],
                )?;
            }
            Ok(Some(id))
        }
        Kind::GasTooHigh | Kind::TxPrepareFailed | Kind::SubmitFailed => {
            stats.topics.insert("requests");
            let reason = reason.unwrap_or("Tx/Unknown");
            let max_stage = if kind == Kind::SubmitFailed { 5 } else { 4 };
            let prefer = if kind == Kind::SubmitFailed { 3 } else { 1 };
            match oldest_pending(tx, chain, max_stage, prefer, l.ts, None)? {
                Some(id) => conclude(tx, id, l, reason).map(|()| Some(id)),
                None => insert_request(tx, l, Some(chain), 3, outcome_for_reason(reason), Some(reason)).map(Some),
            }
        }
        Kind::TxBroadcasted => {
            let Some(id) = oldest_pending(tx, chain, 5, 3, l.ts, None)? else {
                return Ok(None);
            };
            let client = l.field("rpc").unwrap_or("?");
            tx.execute(
                "UPDATE requests SET stage=max(stage, 4), broadcasts=broadcasts+1, updated_ts=?2,
                     rpc_clients=CASE WHEN rpc_clients IS NULL THEN ?3 ELSE rpc_clients || ',' || ?3 END
                 WHERE id=?1",
                params![id, l.ts, client],
            )?;
            Ok(Some(id))
        }
        Kind::TxSubmitted => {
            stats.topics.insert("requests");
            stats.topics.insert("relays");
            let Some(hash) = l.field("tx_hash").map(str::to_lowercase).filter(|h| redact::is_tx_hash(h)) else {
                return Ok(None);
            };
            let id = tx
                .query_row(
                    "SELECT id FROM requests
                     WHERE outcome='pending' AND chain_id=?1 AND ts >= ?2 - ?3 AND ts <= ?2
                     ORDER BY (stage >= 3) DESC, ts ASC, id ASC LIMIT 1",
                    params![chain as i64, l.ts, PENDING_TIMEOUT_US],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?;
            let req = match id {
                Some(id) => {
                    tx.execute(
                        "UPDATE requests SET stage=5, outcome='submitted', tx_hash=?2, updated_ts=?3 WHERE id=?1",
                        params![id, hash, l.ts],
                    )?;
                    tx.query_row(
                        "SELECT fee_token, fee_amount, refund_wei, est_cost_wei FROM requests WHERE id=?1",
                        [id],
                        |r| {
                            Ok((
                                r.get::<_, Option<String>>(0)?,
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, Option<String>>(3)?,
                            ))
                        },
                    )
                    .optional()?
                }
                None => None,
            };
            let (fee_token, fee_amount, refund, est) = req.unwrap_or_default();
            tx.execute(
                "INSERT INTO relays(tx_hash, request_id, ts, chain_id, fee_token, fee_amount, fee_native_wei,
                                    fee_native_source, est_cost_wei, source)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, CASE WHEN ?7 IS NULL THEN NULL ELSE 'log_refund' END, ?8, 'log')
                 ON CONFLICT(tx_hash) DO UPDATE SET
                    request_id=coalesce(relays.request_id, excluded.request_id),
                    ts=min(relays.ts, excluded.ts),
                    fee_token=coalesce(relays.fee_token, excluded.fee_token),
                    fee_amount=coalesce(relays.fee_amount, excluded.fee_amount),
                    fee_native_wei=coalesce(relays.fee_native_wei, excluded.fee_native_wei),
                    fee_native_source=coalesce(relays.fee_native_source, excluded.fee_native_source),
                    est_cost_wei=coalesce(relays.est_cost_wei, excluded.est_cost_wei),
                    source='log'",
                params![hash, id, l.ts, chain as i64, fee_token, fee_amount, refund, est],
            )?;
            Ok(id)
        }
        Kind::FeeNoteSubmitted | Kind::FeeNoteCompleted | Kind::FeeNoteFailed | Kind::FeeNoteWarn => {
            stats.topics.insert("relays");
            let Some(hash) = l.field("tx_hash").map(str::to_lowercase).filter(|h| redact::is_tx_hash(h)) else {
                return Ok(None);
            };
            tx.execute(
                "INSERT INTO relays(tx_hash, ts, chain_id, source) VALUES(?1, ?2, ?3, 'fee_note')
                 ON CONFLICT(tx_hash) DO NOTHING",
                params![hash, l.ts, chain as i64],
            )?;
            let (status, detail): (&str, Option<String>) = match kind {
                Kind::FeeNoteSubmitted => ("submitted", None),
                Kind::FeeNoteCompleted => ("completed", None),
                Kind::FeeNoteFailed => ("failed", reason.map(str::to_string)),
                _ => ("pending", Some(redact::redact_text(&l.message))),
            };
            // Never downgrade a terminal state; `pending` only fills an empty state.
            tx.execute(
                "UPDATE relays SET assurance=?2, assurance_ts=?3, assurance_detail=coalesce(?4, assurance_detail)
                 WHERE tx_hash=?1 AND (
                    assurance IS NULL
                    OR (?2 IN ('completed', 'failed'))
                    OR (?2 = 'submitted' AND assurance = 'pending'))",
                params![hash, status, l.ts, detail],
            )?;
            if kind == Kind::FeeNoteWarn {
                tx.execute(
                    "UPDATE relays SET assurance_detail=?2 WHERE tx_hash=?1 AND assurance NOT IN ('completed', 'failed')",
                    params![hash, redact::redact_text(&l.message)],
                )?;
            }
            Ok(tx
                .query_row("SELECT request_id FROM relays WHERE tx_hash=?1", [&hash], |r| r.get(0))
                .optional()?
                .flatten())
        }
        Kind::OurFees => {
            stats.topics.insert("fees");
            if let Some(json) = l.field("payload").and_then(parse::unwrap_ok_string) {
                apply_our_fees(tx, chain, l.ts, &json)?;
            }
            Ok(None)
        }
        Kind::ServiceStart => {
            let recent: Option<(i64, String)> = tx
                .query_row(
                    "SELECT ts, chains FROM restarts WHERE ts >= ?1 - 120000000 AND ts <= ?1 ORDER BY ts DESC LIMIT 1",
                    [l.ts],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match recent {
                Some((ts, chains)) => {
                    let mut set: BTreeSet<String> = chains.split(',').map(str::to_string).collect();
                    set.insert(chain.to_string());
                    tx.execute(
                        "UPDATE restarts SET chains=?2 WHERE ts=?1",
                        params![ts, set.into_iter().collect::<Vec<_>>().join(",")],
                    )?;
                }
                None => {
                    tx.execute(
                        "INSERT OR IGNORE INTO restarts(ts, chains) VALUES(?1, ?2)",
                        params![l.ts, chain.to_string()],
                    )?;
                }
            }
            tx.execute(
                "INSERT INTO chain_state(chain_id, last_start) VALUES(?1, ?2)
                 ON CONFLICT(chain_id) DO UPDATE SET last_start=max(coalesce(last_start, 0), excluded.last_start)",
                params![chain as i64, l.ts],
            )?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn apply_our_fees(tx: &Transaction<'_>, chain: u64, ts: i64, json: &str) -> Result<()> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Ok(());
    };
    if let Some(fees) = v.get("fees").and_then(Value::as_object) {
        for (token, rate) in fees {
            let Some(rate) = rate.as_str().and_then(parse_uint) else { continue };
            let token = token.to_lowercase();
            let rate = rate.to_string();
            tx.execute(
                "INSERT INTO our_fees(chain_id, token, rate, ts) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(chain_id, token) DO UPDATE SET rate=excluded.rate, ts=excluded.ts
                 WHERE excluded.ts >= our_fees.ts",
                params![chain as i64, token, rate, ts],
            )?;
            let last: Option<i64> = tx
                .query_row(
                    "SELECT max(ts) FROM our_fee_history WHERE chain_id=?1 AND token=?2",
                    params![chain as i64, token],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if last.is_none_or(|t| ts - t >= OUR_FEE_HISTORY_US || ts < t) {
                tx.execute(
                    "INSERT OR IGNORE INTO our_fee_history(chain_id, token, ts, rate) VALUES(?1, ?2, ?3, ?4)",
                    params![chain as i64, token, ts, rate],
                )?;
            }
        }
    }
    tx.execute(
        "INSERT INTO chain_state(chain_id, last_fee_publish, available_wallets, version) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(chain_id) DO UPDATE SET
            available_wallets=CASE WHEN excluded.last_fee_publish >= coalesce(last_fee_publish, 0) THEN excluded.available_wallets ELSE available_wallets END,
            version=CASE WHEN excluded.last_fee_publish >= coalesce(last_fee_publish, 0) THEN excluded.version ELSE version END,
            last_fee_publish=max(coalesce(last_fee_publish, 0), excluded.last_fee_publish)",
        params![
            chain as i64,
            ts,
            v.get("availableWallets").and_then(Value::as_i64),
            v.get("version").and_then(Value::as_str)
        ],
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn rand_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn db() -> crate::db::Db {
        crate::db::Db::open_in_memory_shared(&format!("ingest{}", rand_id())).unwrap()
    }

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    const H: &str = "0x94a9c12e15fbc85b2000277ebdcd37fe1d92ca9a00bb69c4c972939378d5846b";

    #[test]
    fn first_relay_correlates() {
        let db = db();
        let mut c = db.writer.lock().unwrap();
        let s = ingest_lines(&mut c, &lines(include_str!("../tests/fixtures/first-relay-2026-10-17.log"))).unwrap();
        assert_eq!(s.new_events, 9);
        let (outcome, stage, refund, est, gas, broadcasts): (String, i64, String, String, i64, i64) = c
            .query_row("SELECT outcome, stage, refund_wei, est_cost_wei, gas, broadcasts FROM requests", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .unwrap();
        assert_eq!((outcome.as_str(), stage, gas, broadcasts), ("submitted", 5, 1_308_974, 1));
        assert_eq!(refund, "106983915816351");
        assert_eq!(est, "76937196976306");
        let (hash, chain, token, amount, fee_wei, assurance): (String, i64, String, String, String, String) = c
            .query_row(
                "SELECT tx_hash, chain_id, fee_token, fee_amount, fee_native_wei, assurance FROM relays",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(hash, H);
        assert_eq!(chain, 1);
        assert_eq!(token, "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        assert_eq!(amount, "106983915816351");
        assert_eq!(fee_wei, "106983915816351");
        assert_eq!(assurance, "completed");

        // Replaying is idempotent.
        let s = ingest_lines(&mut c, &lines(include_str!("../tests/fixtures/first-relay-2026-10-17.log"))).unwrap();
        assert_eq!((s.new_events, s.duplicates), (0, 9));
        let n: i64 = c.query_row("SELECT count(*) FROM requests", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn ppoi_rejection_takes_chain_from_parsed_line() {
        let db = db();
        let mut c = db.writer.lock().unwrap();
        ingest_lines(&mut c, &lines(include_str!("../tests/fixtures/ppoi-rejection.log"))).unwrap();
        let rows: Vec<(Option<i64>, String, String)> = c
            .prepare("SELECT chain_id, outcome, reason FROM requests")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(Some(137), "rejected".into(), "Poi/ValidateList/InvalidSnarkProof".into())]);
    }

    #[test]
    fn interleaved_chains() {
        let log = "\
2026-10-17T10:00:00.000000Z  INFO transact{chain_id=1}: parsed transact calldata fee_token=0xaa fee_amount=100 railgun_txid=1 utxo_tree_in=0
2026-10-17T10:00:00.100000Z  INFO transact{chain_id=137}: parsed transact calldata fee_token=0xbb fee_amount=200 railgun_txid=2 utxo_tree_in=0
2026-10-17T10:00:00.200000Z  INFO tx{chain_id=137}: converting value to gas token token=0xbb amount=200
2026-10-17T10:00:00.300000Z  INFO transact{chain_id=42161}: parsed transact calldata fee_token=0xcc fee_amount=300 railgun_txid=3 utxo_tree_in=0
2026-10-17T10:00:00.350000Z  WARN failed to handle transact request error=Poi(ValidateList { list_key: 0x01, source: MerkleRootsRejected })
2026-10-17T10:00:00.400000Z  INFO tx{chain_id=1}: converting value to gas token token=0xaa amount=100
2026-10-17T10:00:00.500000Z  INFO tx{chain_id=137}: estimated gas gas=1 gas_price=1 cost=\"0.10\" refund=\"0.20\"
2026-10-17T10:00:00.600000Z  INFO tx{chain_id=1}: estimated gas gas=1 gas_price=1 cost=\"0.30\" refund=\"0.20\"
2026-10-17T10:00:00.610000Z  WARN tx{chain_id=1}: gas cost is too high, ignoring the transact request...
2026-10-17T10:00:00.700000Z  INFO tx{chain_id=137}: tx broadcasted rpc=\"regular\" tx_hash=0x1111111111111111111111111111111111111111111111111111111111111111
2026-10-17T10:00:00.800000Z  INFO tx{chain_id=137}: submitted tx tx_hash=0x1111111111111111111111111111111111111111111111111111111111111111
2026-10-17T10:00:01.000000Z  WARN failed to handle transact request error=Decrypt(AesGcm(DecryptFailed))
";
        let db = db();
        let mut c = db.writer.lock().unwrap();
        ingest_lines(&mut c, &lines(log)).unwrap();
        type Row = (Option<i64>, Option<String>, String, Option<String>);
        let rows: Vec<Row> = c
            .prepare("SELECT chain_id, fee_token, outcome, reason FROM requests ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (Some(1), Some("0xaa".into()), "rejected".into(), Some("Tx/GasCostTooHigh".into())),
                (Some(137), Some("0xbb".into()), "submitted".into(), None),
                (
                    Some(42161),
                    Some("0xcc".into()),
                    "rejected".into(),
                    Some("Poi/ValidateList/MerkleRootsRejected".into())
                ),
                (None, None, "rejected".into(), Some("Decrypt/AesGcm/DecryptFailed".into())),
            ]
        );
        let relay: (i64, String) =
            c.query_row("SELECT chain_id, fee_native_wei FROM relays", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(relay, (137, "200000000000000000".into()));
    }

    #[test]
    fn stale_pending_request_does_not_steal_lines() {
        // Request 1's outcome line is missing; request 2 must still get its own tx lines.
        let log = "\
2026-10-17T10:00:00.000000Z  INFO transact{chain_id=137}: parsed transact calldata fee_token=0xaa fee_amount=100 railgun_txid=1 utxo_tree_in=0
2026-10-17T10:01:00.000000Z  INFO transact{chain_id=137}: parsed transact calldata fee_token=0xbb fee_amount=200 railgun_txid=2 utxo_tree_in=0
2026-10-17T10:01:00.100000Z  INFO tx{chain_id=137}: converting value to gas token token=0xbb amount=200
2026-10-17T10:01:00.200000Z  INFO tx{chain_id=137}: estimated gas gas=1 gas_price=1 cost=\"0.10\" refund=\"0.20\"
2026-10-17T10:01:00.300000Z  INFO tx{chain_id=137}: tx broadcasted rpc=\"regular\" tx_hash=0x2222222222222222222222222222222222222222222222222222222222222222
2026-10-17T10:01:00.400000Z  INFO tx{chain_id=137}: submitted tx tx_hash=0x2222222222222222222222222222222222222222222222222222222222222222
";
        let db = db();
        let mut c = db.writer.lock().unwrap();
        ingest_lines(&mut c, &lines(log)).unwrap();
        let rows: Vec<(String, String, i64)> = c
            .prepare("SELECT fee_token, outcome, stage FROM requests ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![("0xaa".into(), "pending".into(), 1), ("0xbb".into(), "submitted".into(), 5)]);
    }

    #[test]
    fn pending_requests_time_out_in_log_time() {
        let log = "\
2026-10-17T10:00:00.000000Z  INFO transact{chain_id=1}: parsed transact calldata fee_token=0xaa fee_amount=100 railgun_txid=1 utxo_tree_in=0
2026-10-17T10:20:00.000000Z DEBUG fees{chain_id=1}: our fees payload=Ok(\"{}\")
";
        let db = db();
        let mut c = db.writer.lock().unwrap();
        ingest_lines(&mut c, &lines(log)).unwrap();
        let outcome: String = c.query_row("SELECT outcome FROM requests", [], |r| r.get(0)).unwrap();
        assert_eq!(outcome, "unknown");
    }

    #[test]
    fn our_fees_health_and_restarts() {
        let db = db();
        let mut c = db.writer.lock().unwrap();
        let s = ingest_lines(&mut c, &lines(include_str!("../tests/fixtures/our-fees-polygon.log"))).unwrap();
        assert_eq!(s.unparsed, 1);
        let rate: String = c
            .query_row(
                "SELECT rate FROM our_fees WHERE chain_id=137 AND token='0x3c499c542cef5e3811e1192ce70d8cc03d5c3359'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rate, "98123");
        let (publish, wallets): (i64, i64) = c
            .query_row("SELECT last_fee_publish, available_wallets FROM chain_state WHERE chain_id=137", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(wallets, 1);
        assert_eq!(publish, crate::parse::parse_ts("2026-10-17T21:14:56.184940Z").unwrap());
        let restarts: Vec<(i64, String)> = c
            .prepare("SELECT ts, chains FROM restarts")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(restarts.len(), 1);
        assert_eq!(restarts[0].1, "1,137");
        let warns: i64 = c
            .query_row("SELECT sum(count) FROM hourly_counters WHERE kind IN ('sync_warn','price_warn')", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(warns, 2);
        // The payload itself is not stored raw.
        let f: Option<String> =
            c.query_row("SELECT fields FROM events WHERE kind='our_fees'", [], |r| r.get(0)).unwrap();
        assert!(!f.unwrap_or_default().contains("payload"));
    }
}
