//! SQLite storage (WAL). One writer connection and one reader connection.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- One row per ingested log line (deduplicated by hash). `fields` is redacted JSON.
CREATE TABLE IF NOT EXISTS events(
    id             INTEGER PRIMARY KEY,
    hash           INTEGER NOT NULL UNIQUE,
    ts             INTEGER NOT NULL,          -- unix µs (log timestamp)
    level          TEXT NOT NULL,
    chain_id       INTEGER,
    kind           TEXT NOT NULL,
    message        TEXT NOT NULL,
    fields         TEXT,
    parser_version INTEGER NOT NULL,
    request_id     INTEGER
);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
CREATE INDEX IF NOT EXISTS events_kind_ts ON events(kind, ts);

-- Correlated transact requests. stage: 1 parsed, 2 converting, 3 estimated, 4 broadcasted, 5 submitted.
-- outcome: pending | submitted | rejected | failed | unknown
CREATE TABLE IF NOT EXISTS requests(
    id           INTEGER PRIMARY KEY,
    ts           INTEGER NOT NULL,
    chain_id     INTEGER,
    fee_token    TEXT,
    fee_amount   TEXT,
    railgun_txid TEXT,
    stage        INTEGER NOT NULL,
    outcome      TEXT NOT NULL,
    reason       TEXT,
    detail       TEXT,
    gas          INTEGER,
    gas_price    TEXT,
    est_cost_wei TEXT,
    refund_wei   TEXT,
    broadcasts   INTEGER NOT NULL DEFAULT 0,
    rpc_clients  TEXT,
    tx_hash      TEXT,
    updated_ts   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS requests_open ON requests(outcome, chain_id, ts);
CREATE INDEX IF NOT EXISTS requests_ts ON requests(ts);

CREATE TABLE IF NOT EXISTS relays(
    tx_hash             TEXT PRIMARY KEY,
    request_id          INTEGER,
    ts                  INTEGER NOT NULL,
    chain_id            INTEGER NOT NULL,
    fee_token           TEXT,
    fee_amount          TEXT,
    fee_native_wei      TEXT,
    fee_native_source   TEXT,
    est_cost_wei        TEXT,
    receipt_state       TEXT NOT NULL DEFAULT 'pending',   -- pending | confirmed | not_found
    block               INTEGER,
    status              INTEGER,
    gas_used            INTEGER,
    effective_gas_price TEXT,
    cost_wei            TEXT,
    tx_to               TEXT,
    route               TEXT,
    fee_native          REAL,
    cost_native         REAL,
    profit_native       REAL,
    native_usd          REAL,
    fee_usd             REAL,
    cost_usd            REAL,
    profit_usd          REAL,
    assurance           TEXT,
    assurance_ts        INTEGER,
    assurance_detail    TEXT,
    check_attempts      INTEGER NOT NULL DEFAULT 0,
    next_check_ts       INTEGER NOT NULL DEFAULT 0,
    source              TEXT NOT NULL DEFAULT 'log'
);
CREATE INDEX IF NOT EXISTS relays_ts ON relays(ts);

CREATE TABLE IF NOT EXISTS hourly_counters(
    chain_id INTEGER NOT NULL,   -- 0 = no chain
    kind     TEXT NOT NULL,
    hour     INTEGER NOT NULL,   -- unix seconds / 3600
    count    INTEGER NOT NULL,
    PRIMARY KEY(chain_id, kind, hour)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS chain_state(
    chain_id          INTEGER PRIMARY KEY,
    last_fee_publish  INTEGER,
    last_start        INTEGER,
    last_event        INTEGER,
    last_est_cost_wei TEXT,
    last_est_ts       INTEGER,
    available_wallets INTEGER,
    version           TEXT,
    gas_price_wei     TEXT,
    gas_price_ts      INTEGER
);

CREATE TABLE IF NOT EXISTS our_fees(
    chain_id INTEGER NOT NULL,
    token    TEXT NOT NULL,
    rate     TEXT NOT NULL,
    ts       INTEGER NOT NULL,
    PRIMARY KEY(chain_id, token)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS our_fee_history(
    chain_id INTEGER NOT NULL,
    token    TEXT NOT NULL,
    ts       INTEGER NOT NULL,
    rate     TEXT NOT NULL,
    PRIMARY KEY(chain_id, token, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS restarts(
    ts       INTEGER PRIMARY KEY,
    chains   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS balances(
    chain_id INTEGER NOT NULL,
    ts       INTEGER NOT NULL,
    wei      TEXT NOT NULL,
    native   REAL NOT NULL,
    PRIMARY KEY(chain_id, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS prices(
    chain_id INTEGER NOT NULL,
    ts       INTEGER NOT NULL,
    usd      REAL NOT NULL,
    source   TEXT NOT NULL,
    PRIMARY KEY(chain_id, ts)
) WITHOUT ROWID;

-- Waku broadcasters seen (0zk addresses are long; samples reference this id).
CREATE TABLE IF NOT EXISTS broadcasters(
    id              INTEGER PRIMARY KEY,
    railgun_address TEXT NOT NULL UNIQUE,
    identifier      TEXT,
    first_seen      INTEGER NOT NULL,
    last_seen       INTEGER NOT NULL
);

-- Per-broadcaster rate samples (every `waku.fee_sample_secs`).
CREATE TABLE IF NOT EXISTS fee_samples(
    ts             INTEGER NOT NULL,
    chain_id       INTEGER NOT NULL,
    broadcaster_id INTEGER NOT NULL,
    token          TEXT NOT NULL,
    rate           TEXT NOT NULL,
    reliability    REAL,
    PRIMARY KEY(ts, chain_id, broadcaster_id, token)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS market_snapshots(
    ts             INTEGER NOT NULL,
    chain_id       INTEGER NOT NULL,
    token          TEXT NOT NULL,
    our_rate       TEXT,
    our_rank       INTEGER,
    n_broadcasters INTEGER NOT NULL,
    cheapest       TEXT,
    pool_size      INTEGER NOT NULL,
    in_pool        INTEGER,
    picked         INTEGER,  -- v2: would findBestBroadcaster return us
    eligible       INTEGER,  -- v2: our rate inside the trusted-fee band
    PRIMARY KEY(ts, chain_id, token)
) WITHOUT ROWID;
"#;

#[derive(Clone)]
pub struct Db {
    pub writer: Arc<Mutex<Connection>>,
    pub reader: Arc<Mutex<Connection>>,
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-2000;
         PRAGMA temp_store=MEMORY;
         PRAGMA mmap_size=0;",
    )?;
    Ok(())
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let writer = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        configure(&writer)?;
        writer.execute_batch(SCHEMA)?;
        migrate(&writer)?;
        let reader = Connection::open(path)?;
        configure(&reader)?;
        Ok(Self { writer: Arc::new(Mutex::new(writer)), reader: Arc::new(Mutex::new(reader)) })
    }

    pub fn open_in_memory_shared(name: &str) -> Result<Self> {
        let uri = format!("file:{name}?mode=memory&cache=shared");
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_URI;
        let writer = Connection::open_with_flags(&uri, flags)?;
        writer.execute_batch(SCHEMA)?;
        migrate(&writer)?;
        let reader = Connection::open_with_flags(&uri, flags)?;
        Ok(Self { writer: Arc::new(Mutex::new(writer)), reader: Arc::new(Mutex::new(reader)) })
    }

    /// Run a closure on the writer connection on the blocking pool.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.writer.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut c)
        })
        .await?
    }

    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.reader.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&c)
        })
        .await?
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let v: Option<String> =
        conn.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).optional()?;
    match v.and_then(|v| v.parse::<i64>().ok()) {
        None => {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )?;
        }
        Some(v) if v > SCHEMA_VERSION => {
            anyhow::bail!("database schema {v} is newer than this binary ({SCHEMA_VERSION})");
        }
        Some(1) => {
            // v2: wallet-pick model (trusted-fee band + cheapest) next to the random pool.
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE market_snapshots ADD COLUMN picked INTEGER;
                 ALTER TABLE market_snapshots ADD COLUMN eligible INTEGER;
                 UPDATE meta SET value='2' WHERE key='schema_version';
                 COMMIT;",
            )?;
        }
        Some(_) => {}
    }
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn now_us() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1000).unwrap_or(0)
}

/// Delete old rows according to the retention settings.
pub fn prune(conn: &Connection, storage: &crate::config::Storage) -> Result<usize> {
    let now = now_us();
    let day = 86_400_000_000i64;
    let raw_cut = now - i64::from(storage.raw_events_days) * day;
    let noisy: Vec<&str> = NOISY_KINDS.to_vec();
    let placeholders = noisy.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut n = 0;
    let sql = format!("DELETE FROM events WHERE ts < ? AND kind IN ({placeholders})");
    let mut args: Vec<rusqlite::types::Value> = vec![raw_cut.into()];
    args.extend(noisy.iter().map(|k| rusqlite::types::Value::from((*k).to_string())));
    n += conn.execute(&sql, rusqlite::params_from_iter(args))?;
    let fee_cut = now - i64::from(storage.fee_history_days) * day;
    n += conn.execute("DELETE FROM fee_samples WHERE ts < ?1", [fee_cut])?;
    let mkt_cut = now - i64::from(storage.market_history_days) * day;
    n += conn.execute("DELETE FROM market_snapshots WHERE ts < ?1", [mkt_cut])?;
    n += conn.execute("DELETE FROM our_fee_history WHERE ts < ?1", [mkt_cut])?;
    n += conn.execute("DELETE FROM prices WHERE ts < ?1", [mkt_cut])?;
    Ok(n)
}

pub const NOISY_KINDS: &[&str] = &[
    "our_fees",
    "sync_warn",
    "price_warn",
    "channel_full",
    "poi_fallback",
    "fee_note_info",
    "other",
    "unparsed",
    "waku_warn",
    "maintenance",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_v1_market_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rbm.sqlite");
        drop(Db::open(&path).unwrap());
        // Turn it back into a v1 database (as deployed with 0.1.0).
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "ALTER TABLE market_snapshots DROP COLUMN picked;
             ALTER TABLE market_snapshots DROP COLUMN eligible;
             INSERT INTO market_snapshots(ts, chain_id, token, n_broadcasters, pool_size, in_pool) VALUES(1, 137, 't', 3, 1, 0);
             UPDATE meta SET value='1' WHERE key='schema_version';",
        )
        .unwrap();
        drop(c);
        let db = Db::open(&path).unwrap();
        let c = db.writer.lock().unwrap();
        assert_eq!(get_meta(&c, "schema_version").unwrap().as_deref(), Some("2"));
        let (n, picked): (i64, Option<i64>) = c
            .query_row("SELECT count(*), max(picked) FROM market_snapshots", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((n, picked), (1, None));
    }

    #[test]
    fn noisy_kinds_match_parser() {
        use crate::parse::Kind::*;
        for k in [
            OurFees,
            SyncWarn,
            PriceWarn,
            ChannelFull,
            PoiFallback,
            FeeNoteInfo,
            Other,
            Unparsed,
            WakuWarn,
            Maintenance,
        ] {
            assert!(k.is_noisy());
            assert!(NOISY_KINDS.contains(&k.as_str()));
        }
    }

    #[test]
    fn opens_and_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("x.sqlite")).unwrap();
        let c = db.writer.lock().unwrap();
        assert_eq!(get_meta(&c, "schema_version").unwrap().as_deref(), Some("2"));
    }
}
