//! Minimal JSON-RPC client: endpoint rotation, per-endpoint cooldown after errors /
//! rate limits, small batches, explicit User-Agent (publicnode returns 403 without one).

use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config;

pub const MAX_BATCH: usize = 10;

struct Endpoint {
    url: String,
    bad_until: Mutex<Option<Instant>>,
}

pub struct RpcPool {
    pub chain_id: u64,
    endpoints: Vec<Endpoint>,
    client: reqwest::Client,
    cooldown: Duration,
    next: AtomicUsize,
}

#[derive(Debug, Clone)]
pub struct Call {
    pub method: &'static str,
    pub params: Value,
}

impl Call {
    pub fn new(method: &'static str, params: Value) -> Self {
        Self { method, params }
    }
}

impl RpcPool {
    pub fn new(chain_id: u64, urls: &[String], rpc: &config::Rpc) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(rpc.user_agent.clone())
            .timeout(Duration::from_secs(rpc.timeout_secs))
            .connect_timeout(Duration::from_secs(rpc.timeout_secs.min(5)))
            .pool_max_idle_per_host(1)
            .build()?;
        Ok(Self {
            chain_id,
            endpoints: urls.iter().map(|u| Endpoint { url: u.clone(), bad_until: Mutex::new(None) }).collect(),
            client,
            cooldown: Duration::from_secs(rpc.cooldown_secs),
            next: AtomicUsize::new(0),
        })
    }

    /// Endpoints in try-order: healthy ones round-robin first, cooling-down ones last.
    fn order(&self) -> Vec<usize> {
        let n = self.endpoints.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % n.max(1);
        let now = Instant::now();
        let (mut ok, mut bad) = (Vec::new(), Vec::new());
        for k in 0..n {
            let i = (start + k) % n;
            let cool = self.endpoints[i].bad_until.lock().unwrap().is_some_and(|t| t > now);
            if cool { bad.push(i) } else { ok.push(i) }
        }
        ok.extend(bad);
        ok
    }

    fn mark_bad(&self, i: usize) {
        *self.endpoints[i].bad_until.lock().unwrap() = Some(Instant::now() + self.cooldown);
    }

    /// Execute a batch; returns per-call results (JSON-RPC errors are per call).
    pub async fn batch(&self, calls: &[Call]) -> Result<Vec<Result<Value>>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(MAX_BATCH) {
            out.extend(self.batch_once(chunk).await?);
        }
        Ok(out)
    }

    async fn batch_once(&self, calls: &[Call]) -> Result<Vec<Result<Value>>> {
        let body: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| json!({"jsonrpc": "2.0", "id": i, "method": c.method, "params": c.params}))
            .collect();
        let mut last_err = anyhow!("no rpc endpoints for chain {}", self.chain_id);
        for i in self.order() {
            let ep = &self.endpoints[i];
            match self.post(&ep.url, &body).await {
                Ok(resp) => match split_batch(resp, calls.len()) {
                    Ok(results) => {
                        // A rate-limit error on every call means the endpoint is unhappy: rotate.
                        if results.iter().all(|r| r.as_ref().is_err_and(|e| is_rate_limit(&e.to_string()))) {
                            self.mark_bad(i);
                            last_err = anyhow!("{}: rate limited", redact_url(&ep.url));
                            continue;
                        }
                        return Ok(results);
                    }
                    Err(e) => {
                        self.mark_bad(i);
                        last_err = e;
                    }
                },
                Err(e) => {
                    self.mark_bad(i);
                    tracing::debug!(chain = self.chain_id, url = %redact_url(&ep.url), error = %e, "rpc error");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn post(&self, url: &str, body: &[Value]) -> Result<Value> {
        let resp = self.client.post(url).json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("{}: HTTP {status}", redact_url(url));
        }
        Ok(resp.json::<Value>().await?)
    }

    pub async fn call(&self, method: &'static str, params: Value) -> Result<Value> {
        self.batch(&[Call::new(method, params)]).await?.pop().ok_or_else(|| anyhow!("empty batch response"))?
    }
}

fn is_rate_limit(s: &str) -> bool {
    let s = s.to_lowercase();
    s.contains("rate limit") || s.contains("limit exceeded") || s.contains("too many requests") || s.contains("-32005")
}

fn split_batch(resp: Value, n: usize) -> Result<Vec<Result<Value>>> {
    let items = match resp {
        Value::Array(a) => a,
        // Some endpoints answer a batch with a single error object.
        Value::Object(o) => {
            let msg = o.get("error").map(Value::to_string).unwrap_or_else(|| "unexpected response".into());
            bail!("batch rejected: {msg}");
        }
        _ => bail!("unexpected batch response"),
    };
    let mut out: Vec<Option<Result<Value>>> = (0..n).map(|_| None).collect();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_u64).map(|i| i as usize) else { continue };
        if id >= n {
            continue;
        }
        out[id] = Some(match item.get("error") {
            Some(e) if !e.is_null() => Err(anyhow!("rpc error: {e}")),
            _ => Ok(item.get("result").cloned().unwrap_or(Value::Null)),
        });
    }
    Ok(out.into_iter().map(|r| r.unwrap_or_else(|| Err(anyhow!("missing response")))).collect())
}

/// Strip credentials, path and query (API keys often live there) for logging.
pub fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?']).next().unwrap_or(rest);
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            format!("{scheme}://{host}")
        }
        None => "<rpc>".into(),
    }
}

pub fn hex_u64(v: &Value) -> Option<u64> {
    u64::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_path_and_query() {
        assert_eq!(redact_url("https://eth-mainnet.example.com/v2/SECRETKEY"), "https://eth-mainnet.example.com");
        assert_eq!(redact_url("https://rpc.example.com/?apikey=SECRETKEY"), "https://rpc.example.com");
        assert_eq!(redact_url("https://user:SECRET@rpc.example.com:8545/path"), "https://rpc.example.com:8545");
        assert_eq!(redact_url("not a url"), "<rpc>");
    }

    #[test]
    fn splits_out_of_order_batches() {
        let resp = json!([
            {"jsonrpc":"2.0","id":1,"result":"0x2"},
            {"jsonrpc":"2.0","id":0,"error":{"code":-32000,"message":"x"}}
        ]);
        let r = split_batch(resp, 3).unwrap();
        assert!(r[0].is_err());
        assert_eq!(r[1].as_ref().unwrap(), "0x2");
        assert!(r[2].is_err());
        assert_eq!(redact_url("https://eth-mainnet.g.alchemy.com/v2/KEY"), "https://eth-mainnet.g.alchemy.com");
    }
}
