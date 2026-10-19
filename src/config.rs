//! Dashboard configuration (TOML). Contains public data only: addresses, RPC URLs,
//! oracle addresses. There is deliberately no field that could hold key material.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub broadcaster: Broadcaster,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub web: Web,
    #[serde(default)]
    pub storage: Storage,
    #[serde(default)]
    pub waku: Waku,
    #[serde(default)]
    pub health: Health,
    #[serde(default)]
    pub rpc: Rpc,
    #[serde(rename = "chain", default)]
    pub chains: Vec<Chain>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Broadcaster {
    /// Display label.
    #[serde(default = "default_name")]
    pub name: String,
    /// Public EVM gas wallet address (same on all chains).
    pub gas_wallet: String,
    /// Public 0zk address; used to recognise our own fee messages on Waku.
    pub railgun_address: String,
}

fn default_name() -> String {
    "broadcaster".into()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// Read lines from stdin (`docker logs -f … 2>&1 | rbm ingest`).
    #[default]
    Stdin,
    /// Spawn a command and read its stdout+stderr; restarted when it exits.
    Command,
    /// `journalctl -f -o json` filtered by `journald_match`.
    Journald,
    /// No log source (web/enrichment only, e.g. after `rbm replay`).
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    #[serde(default)]
    pub kind: SourceKind,
    /// Command argv. `{since}` is replaced with the RFC3339 timestamp of the last
    /// ingested line (minus a small overlap) or `10m` on first start.
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    #[serde(default = "default_journald_match")]
    pub journald_match: Vec<String>,
    #[serde(default = "default_journalctl")]
    pub journalctl: String,
    #[serde(default = "default_restart_delay")]
    pub restart_delay_secs: u64,
}

fn default_command() -> Vec<String> {
    ["docker", "logs", "-f", "--since", "{since}", "railgun-broadcaster"].map(String::from).to_vec()
}
fn default_journald_match() -> Vec<String> {
    vec!["CONTAINER_NAME=railgun-broadcaster".into()]
}
fn default_journalctl() -> String {
    "journalctl".into()
}
fn default_restart_delay() -> u64 {
    5
}

impl Default for Source {
    fn default() -> Self {
        Self {
            kind: SourceKind::default(),
            command: default_command(),
            journald_match: default_journald_match(),
            journalctl: default_journalctl(),
            restart_delay_secs: default_restart_delay(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// `user:password`. Can also be given via env `RBM_BASIC_AUTH`.
    pub basic_auth: Option<String>,
    /// Allow binding a non-loopback address without auth. Don't.
    #[serde(default)]
    pub allow_unauthenticated_public_bind: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}
fn default_true() -> bool {
    true
}

impl Default for Web {
    fn default() -> Self {
        Self { bind: default_bind(), basic_auth: None, allow_unauthenticated_public_bind: false, enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    #[serde(default = "default_db_path")]
    pub path: PathBuf,
    /// Raw rows of high-volume event kinds (fee publish, sync warnings) are pruned
    /// after this many days. Their hourly counts are kept forever.
    #[serde(default = "default_raw_days")]
    pub raw_events_days: u32,
    /// Per-broadcaster Waku fee samples.
    #[serde(default = "default_fee_history_days")]
    pub fee_history_days: u32,
    /// Per-chain/token market summaries (rank, pool membership).
    #[serde(default = "default_market_days")]
    pub market_history_days: u32,
}

fn default_db_path() -> PathBuf {
    "rbm.sqlite".into()
}
fn default_raw_days() -> u32 {
    14
}
fn default_fee_history_days() -> u32 {
    7
}
fn default_market_days() -> u32 {
    365
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            raw_events_days: default_raw_days(),
            fee_history_days: default_fee_history_days(),
            market_history_days: default_market_days(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Waku {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_peers")]
    pub max_peers: usize,
    /// How often per-chain/token market summaries (rank, pool membership) are written.
    #[serde(default = "default_sample_secs")]
    pub sample_secs: u64,
    /// How often every broadcaster's rate is written (history for fee tuning).
    #[serde(default = "default_fee_sample_secs")]
    pub fee_sample_secs: u64,
    /// Random-selection pool factor (`findRandomBroadcasterForToken`: cheapest × 1.05).
    /// Railway and Terminal Wallet use `findBestBroadcaster` (cheapest only) instead.
    #[serde(default = "default_pool_factor")]
    pub pool_factor: f64,
    /// 0zk addresses wallets trust as fee signers (Railway remote config `trustedFeeSigner`).
    /// Wallets ignore offers outside [avg × (1 − lower), avg × (1 + upper)] of these signers'
    /// average fee, and show no broadcaster for a token none of them prices. Empty = no filter.
    #[serde(default = "default_trusted_fee_signers")]
    pub trusted_fee_signers: Vec<String>,
    #[serde(default = "default_trusted_fee_lower")]
    pub trusted_fee_lower: f64,
    #[serde(default = "default_trusted_fee_upper")]
    pub trusted_fee_upper: f64,
    /// Optional extra peers: `{ peer_id = "16Uiu2…", addrs = ["/dns4/…/tcp/8000/wss"] }`.
    #[serde(default)]
    pub direct_peers: Vec<WakuPeer>,
    /// Override the DNS ENR tree(s) used for discovery.
    pub dns_enr_trees: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WakuPeer {
    pub peer_id: String,
    pub addrs: Vec<String>,
}

fn default_max_peers() -> usize {
    6
}
fn default_sample_secs() -> u64 {
    600
}
fn default_fee_sample_secs() -> u64 {
    1800
}
fn default_pool_factor() -> f64 {
    1.05
}
/// https://www.railway.xyz/config/railway-config-v3.3.json, 2026-10-19.
fn default_trusted_fee_signers() -> Vec<String> {
    [
        "0zk1qyjyhqjdkqd9qxusgj092ppxl92plvrk3s3cna9u73h5rwt0ghxvfrv7j6fe3z53l7lrzyqw5te7ku5v8fsrpeadzvpkudgawjv9dg08htj7z3mph5kd6dw50jc",
        "0zk1qyzgh9ctuxm6d06gmax39xutjgrawdsljtv80lqnjtqp3exxayuf0rv7j6fe3z53laetcl9u3cma0q9k4npgy8c8ga4h6mx83v09m8ewctsekw4a079dcl5sw4k",
        "0zk1qyqhtwaa9zj3ug9dmxhfedappvm509w7dr5lgadaehxz38w9u457mrv7j6fe3z53layes62mktxj5kd6reh2kxd39ds2gnpf6wphtw39y5g36lsvukeywfqa8y0",
        "0zk1qy88aamk4dp3rn2dvfdu5u8vvtfs89vg8h6zyajr4g5mq0ykm28e0rv7j6fe3z53l7zpahc5w52u8juzg54ypn24slqsyy3dy57s5k3669dyg3jxyp6czxszfs7",
    ]
    .map(String::from)
    .to_vec()
}
fn default_trusted_fee_lower() -> f64 {
    0.10
}
fn default_trusted_fee_upper() -> f64 {
    0.30
}

impl Default for Waku {
    fn default() -> Self {
        Self {
            enabled: true,
            max_peers: default_max_peers(),
            sample_secs: default_sample_secs(),
            fee_sample_secs: default_fee_sample_secs(),
            pool_factor: default_pool_factor(),
            trusted_fee_signers: default_trusted_fee_signers(),
            trusted_fee_lower: default_trusted_fee_lower(),
            trusted_fee_upper: default_trusted_fee_upper(),
            direct_peers: Vec::new(),
            dns_enr_trees: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    #[serde(default = "default_fee_warn")]
    pub fee_publish_warn_secs: i64,
    #[serde(default = "default_fee_stale")]
    pub fee_publish_stale_secs: i64,
    /// Optional argv printing `docker inspect` JSON for the container, e.g.
    /// `["docker", "inspect", "railgun-broadcaster"]`. Docker access is
    /// root-equivalent, so this is off by default.
    #[serde(default)]
    pub container_inspect: Vec<String>,
    #[serde(default = "default_balance_secs")]
    pub balance_poll_secs: u64,
    #[serde(default = "default_price_secs")]
    pub price_poll_secs: u64,
}

fn default_fee_warn() -> i64 {
    180
}
fn default_fee_stale() -> i64 {
    600
}
fn default_balance_secs() -> u64 {
    300
}
fn default_price_secs() -> u64 {
    300
}

impl Default for Health {
    fn default() -> Self {
        Self {
            fee_publish_warn_secs: default_fee_warn(),
            fee_publish_stale_secs: default_fee_stale(),
            container_inspect: Vec::new(),
            balance_poll_secs: default_balance_secs(),
            price_poll_secs: default_price_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rpc {
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Cooldown for an endpoint after an error / rate limit.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

fn default_user_agent() -> String {
    concat!("railgun-broadcaster-monitor/", env!("CARGO_PKG_VERSION")).into()
}
fn default_timeout() -> u64 {
    10
}
fn default_cooldown() -> u64 {
    60
}

impl Default for Rpc {
    fn default() -> Self {
        Self { user_agent: default_user_agent(), timeout_secs: default_timeout(), cooldown_secs: default_cooldown() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    pub id: u64,
    pub name: String,
    pub native_symbol: String,
    /// Explorer URL prefix for transactions, e.g. `https://etherscan.io/tx/`.
    pub explorer_tx: String,
    #[serde(default)]
    pub explorer_address: Option<String>,
    pub rpcs: Vec<String>,
    /// RelayAdapt contract(s). A relay tx sent to one of these went through the `mev` queue.
    #[serde(default)]
    pub relay_adapt: Vec<String>,
    /// Our configured `fee_bonus` as a fraction (0.048 = 4.8 %). Public info; used to
    /// convert published rates into a "× market" multiplier.
    pub fee_bonus: Option<f64>,
    /// Chainlink aggregator for native/USD (8 decimals assumed via `decimals()`).
    pub native_usd_feed: Option<String>,
    /// Balance below which the health card turns amber (in native units).
    pub low_balance: Option<f64>,
    #[serde(rename = "token", default)]
    pub tokens: Vec<Token>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub address: String,
    pub symbol: String,
    pub decimals: u32,
    /// Treat as 1 USD (used for USD fallbacks).
    #[serde(default)]
    pub stable: bool,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        if cfg.web.basic_auth.is_none()
            && let Ok(v) = std::env::var("RBM_BASIC_AUTH")
            && !v.is_empty()
        {
            cfg.web.basic_auth = Some(v);
        }
        cfg.normalize();
        cfg.validate()?;
        cfg.reject_example_values()?;
        Ok(cfg)
    }

    /// `config.example.toml` ships placeholder addresses. Refusing them at load time turns a
    /// forgotten edit into a clear error instead of a dashboard that silently watches nothing.
    pub fn reject_example_values(&self) -> Result<()> {
        if self.broadcaster.gas_wallet == EXAMPLE_GAS_WALLET
            || self.broadcaster.railgun_address == EXAMPLE_RAILGUN_ADDRESS
        {
            bail!(
                "broadcaster.gas_wallet / broadcaster.railgun_address still hold the placeholder values \
                 from config.example.toml: set your broadcaster's own gas wallet and 0zk address"
            );
        }
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.broadcaster.gas_wallet = self.broadcaster.gas_wallet.to_lowercase();
        for c in &mut self.chains {
            for a in &mut c.relay_adapt {
                *a = a.to_lowercase();
            }
            for t in &mut c.tokens {
                t.address = t.address.to_lowercase();
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !is_address(&self.broadcaster.gas_wallet) {
            bail!("broadcaster.gas_wallet must be a 0x address");
        }
        if !self.broadcaster.railgun_address.starts_with("0zk1") {
            bail!("broadcaster.railgun_address must be a 0zk1… address");
        }
        let mut seen = HashMap::new();
        for c in &self.chains {
            if seen.insert(c.id, ()).is_some() {
                bail!("chain {} configured twice", c.id);
            }
            if c.rpcs.is_empty() {
                bail!("chain {} has no rpcs", c.id);
            }
            for t in &c.tokens {
                if !is_address(&t.address) {
                    bail!("chain {}: token {} has an invalid address", c.id, t.symbol);
                }
            }
        }
        if let Some(auth) = &self.web.basic_auth
            && !auth.contains(':')
        {
            bail!("web.basic_auth must be `user:password`");
        }
        if !self.web.bind.ip().is_loopback()
            && self.web.basic_auth.is_none()
            && !self.web.allow_unauthenticated_public_bind
        {
            bail!(
                "web.bind {} is not loopback: set web.basic_auth (or RBM_BASIC_AUTH), \
                 or use an SSH tunnel",
                self.web.bind
            );
        }
        Ok(())
    }

    pub fn chain(&self, id: u64) -> Option<&Chain> {
        self.chains.iter().find(|c| c.id == id)
    }

    pub fn chain_name(&self, id: Option<i64>) -> String {
        match id {
            Some(id) => self.chain(id as u64).map(|c| c.name.clone()).unwrap_or_else(|| format!("chain {id}")),
            None => "unknown".into(),
        }
    }

    pub fn token(&self, chain: u64, address: &str) -> Option<&Token> {
        let a = address.to_lowercase();
        self.chain(chain)?.tokens.iter().find(|t| t.address == a)
    }
}

/// Placeholders in `config.example.toml`.
pub const EXAMPLE_GAS_WALLET: &str = "0x1111111111111111111111111111111111111111";
pub const EXAMPLE_RAILGUN_ADDRESS: &str = "0zk1qyqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqj7m8ay";

pub fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let mut cfg: Config = toml::from_str(text).unwrap();
        cfg.normalize();
        cfg.validate().unwrap();
        assert_eq!(cfg.chains.len(), 4);
        assert!(cfg.token(137, "0x3C499c542cEF5E3811e1192ce70d8cC03d5c3359").is_some());
    }

    #[test]
    fn example_placeholders_match_the_constants_and_are_rejected() {
        let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.normalize();
        assert_eq!(cfg.broadcaster.gas_wallet, EXAMPLE_GAS_WALLET);
        assert_eq!(cfg.broadcaster.railgun_address, EXAMPLE_RAILGUN_ADDRESS);
        assert!(cfg.reject_example_values().is_err());
        cfg.broadcaster.gas_wallet = "0x2222222222222222222222222222222222222222".into();
        cfg.broadcaster.railgun_address = "0zk1other".into();
        assert!(cfg.reject_example_values().is_ok());
    }

    #[test]
    fn refuses_public_bind_without_auth() {
        let text = include_str!("../config.example.toml");
        let mut cfg: Config = toml::from_str(text).unwrap();
        cfg.web.bind = "0.0.0.0:8080".parse().unwrap();
        assert!(cfg.validate().is_err());
        cfg.web.basic_auth = Some("a:b".into());
        assert!(cfg.validate().is_ok());
    }
}
