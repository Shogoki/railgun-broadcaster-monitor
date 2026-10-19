//! End-to-end acceptance tests: real log fixtures (`tests/fixtures/`) go through ingest,
//! correlation and enrichment against a mock JSON-RPC endpoint that serves the real on-chain
//! receipt of the fixture relay, and the rendered pages and the database are checked.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{Json, Router, routing::post};
use railgun_broadcaster_monitor::{
    app::App,
    config::{Config, SourceKind},
    db::Db,
    enrich, ingest, web,
};
use regex::Regex;
use rusqlite::params;
use serde_json::{Value, json};

const TX: &str = "0x94a9c12e15fbc85b2000277ebdcd37fe1d92ca9a00bb69c4c972939378d5846b";
const FIRST_RELAY: &str = include_str!("fixtures/first-relay-2026-10-17.log");
const REJECTION: &str = include_str!("fixtures/ppoi-rejection.log");
const FEES: &str = include_str!("fixtures/our-fees-polygon.log");

static N: AtomicUsize = AtomicUsize::new(0);

/// Mock RPC: the real receipt of the first relay (block 26 215 205), sent to RelayAdapt.
async fn mock_rpc() -> SocketAddr {
    async fn handle(Json(body): Json<Value>) -> Json<Value> {
        let calls = body.as_array().cloned().unwrap_or_default();
        let out: Vec<Value> = calls
            .iter()
            .map(|c| {
                let id = c["id"].clone();
                let result = match c["method"].as_str().unwrap_or("") {
                    "eth_getTransactionReceipt" if c["params"][0] == TX => json!({
                        "transactionHash": TX,
                        "blockNumber": format!("0x{:x}", 26_215_205u64),
                        "status": "0x1",
                        "gasUsed": format!("0x{:x}", 1_173_820u64),
                        "effectiveGasPrice": format!("0x{:x}", 58_776_719u64),
                        "to": "0xac9f360ae85469b27aeddeafc579ef2d052ad405",
                    }),
                    "eth_getTransactionReceipt" => Value::Null,
                    "eth_getBalance" => json!("0xb4202c31dc0a0b"),
                    "eth_gasPrice" => json!("0x3b9aca00"),
                    _ => return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "nope"}}),
                };
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            })
            .collect();
        Json(Value::Array(out))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, Router::new().route("/", post(handle))).await.unwrap() });
    addr
}

async fn app(rpc: SocketAddr) -> Arc<App> {
    let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
    cfg.normalize();
    for c in &mut cfg.chains {
        c.rpcs = vec![format!("http://{rpc}/")];
    }
    cfg.validate().unwrap();
    let db = Db::open_in_memory_shared(&format!("acceptance{}", N.fetch_add(1, Ordering::Relaxed))).unwrap();
    Arc::new(App::new(cfg, db, SourceKind::None).unwrap())
}

fn lines(s: &str) -> Vec<String> {
    s.lines().map(str::to_string).collect()
}

async fn ingest_all(app: &App, text: &str) {
    let l = lines(text);
    app.db.write(move |c| ingest::ingest_lines(c, &l)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_relay_end_to_end() {
    let app = app(mock_rpc().await).await;
    ingest_all(&app, FIRST_RELAY).await;
    // A Chainlink ETH/USD sample close to the relay time.
    app.db
        .write(|c| {
            let ts = railgun_broadcaster_monitor::parse::parse_ts("2026-10-17T22:15:00Z").unwrap();
            c.execute("INSERT INTO prices(chain_id, ts, usd, source) VALUES(1, ?1, 2400.0, 'chainlink')", [ts])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(enrich::enrich_receipts_once(&app).await.unwrap(), 1);

    type RelayRow = (String, i64, String, String, f64, f64, f64, String, i64, i64, String, f64);
    let rows: Vec<RelayRow> = app
        .db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT tx_hash, chain_id, fee_token, fee_amount, fee_native, cost_native, profit_native, route,
                        status, block, assurance, profit_usd FROM relays",
            )?;
            Ok(st
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                    ))
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one relay row");
    let (hash, chain, token, amount, fee, cost, profit, route, status, block, assurance, profit_usd) = rows[0].clone();
    assert_eq!(hash, TX);
    assert_eq!(chain, 1);
    assert_eq!(token, "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"); // WETH
    assert_eq!(amount, "106983915816351"); // 0.000106984 WETH
    assert!((fee - 0.000_106_984).abs() < 1e-9, "fee {fee}");
    assert!((cost - 0.000_069_0).abs() < 1e-7, "cost {cost}"); // 1 173 820 × 58 776 719 wei
    assert!((profit - 0.000_038).abs() < 1e-6, "profit {profit}");
    assert!((profit / cost - 0.55).abs() < 0.01, "margin");
    assert_eq!(route, "mev");
    assert_eq!(status, 1);
    assert_eq!(block, 26_215_205);
    assert_eq!(assurance, "completed");
    assert!((profit_usd - profit * 2400.0).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ppoi_rejection() {
    let app = app(mock_rpc().await).await;
    ingest_all(&app, REJECTION).await;
    let rows: Vec<(Option<i64>, String, String)> = app
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT chain_id, outcome, reason FROM requests")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(rows, vec![(Some(137), "rejected".to_string(), "Poi/ValidateList/InvalidSnarkProof".to_string())]);
}

/// A broadcaster `docker compose up -d` recreation: the follower restarts with
/// `--since <last ts − overlap>` / journald cursor, so lines arrive twice and split
/// across batches. Nothing may be lost or duplicated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn survives_restart_with_overlap() {
    let app = app(mock_rpc().await).await;
    let all = lines(FIRST_RELAY);
    let (a, b) = (all[..4].to_vec(), all[2..].to_vec());
    app.db.write(move |c| ingest::ingest_lines(c, &a)).await.unwrap();
    let stats = app.db.write(move |c| ingest::ingest_lines(c, &b)).await.unwrap();
    assert_eq!(stats.duplicates, 2);
    let (events, requests, relays, assurance): (i64, i64, i64, String) = app
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT (SELECT count(*) FROM events), (SELECT count(*) FROM requests),
                        (SELECT count(*) FROM relays), (SELECT assurance FROM relays)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!((events, requests, relays, assurance.as_str()), (9, 1, 1, "completed"));
}

/// "No key material anywhere": the only 0x+64-hex values in a full DB dump are tx hashes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_key_material_in_db() {
    let app = app(mock_rpc().await).await;
    for f in [FIRST_RELAY, REJECTION, FEES] {
        ingest_all(&app, f).await;
    }
    enrich::enrich_receipts_once(&app).await.unwrap();
    let dump: Vec<String> = app
        .db
        .read(|c| {
            let tables: Vec<String> = c
                .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let mut out = Vec::new();
            for t in tables {
                let mut st = c.prepare(&format!("SELECT * FROM {t}"))?;
                let n = st.column_count();
                let mut rows = st.query(params![])?;
                while let Some(r) = rows.next()? {
                    for i in 0..n {
                        if let Ok(Some(s)) = r.get::<_, Option<String>>(i) {
                            out.push(s);
                        }
                    }
                }
            }
            Ok(out)
        })
        .await
        .unwrap();
    let re = Regex::new(r"(?i)(0x)?[0-9a-f]{64,}").unwrap();
    let mut found = 0;
    for s in &dump {
        for m in re.find_iter(s) {
            if m.as_str().bytes().all(|b| b.is_ascii_digit()) {
                continue; // decimal railgun_txid – allowed
            }
            found += 1;
            assert_eq!(m.as_str().to_lowercase(), TX, "unexpected long hex value persisted: {s}");
        }
    }
    assert!(found > 0, "tx hashes are expected in the dump");
    // The example config has no long hex either.
    assert!(!Regex::new(r"0x[0-9a-fA-F]{64}").unwrap().is_match(include_str!("../config.example.toml")));
}

async fn get(addr: SocketAddr, path: &str, auth: Option<&str>) -> (u16, String) {
    let mut req = reqwest::Client::new().get(format!("http://{addr}{path}"));
    if let Some((u, p)) = auth.and_then(|a| a.split_once(':')) {
        req = req.basic_auth(u, Some(p));
    }
    let res = req.send().await.unwrap();
    (res.status().as_u16(), res.text().await.unwrap())
}

async fn serve(app: Arc<App>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_pages_render() {
    let app = app(mock_rpc().await).await;
    for f in [FIRST_RELAY, REJECTION, FEES] {
        ingest_all(&app, f).await;
    }
    enrich::enrich_receipts_once(&app).await.unwrap();
    let addr = serve(app).await;
    let (s, body) = get(addr, "/", None).await;
    assert_eq!(s, 200);
    assert!(body.contains("Polygon"));
    let (s, body) = get(addr, "/relays?range=all", None).await;
    assert_eq!(s, 200);
    assert!(body.contains("0x94a9c1"), "relay row is listed");
    assert!(body.contains("mev"));
    let (s, body) = get(addr, "/requests?range=all", None).await;
    assert_eq!(s, 200);
    assert!(body.contains("Poi/ValidateList/InvalidSnarkProof"));
    for p in ["/profit?range=all", "/profit?bucket=week", "/market", "/events", "/requests/1", "/assets/app.js"] {
        assert_eq!(get(addr, p, None).await.0, 200, "{p}");
    }
    assert_eq!(get(addr, "/assets/nope.js", None).await.0, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_auth_is_enforced() {
    let rpc = mock_rpc().await;
    let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
    cfg.normalize();
    cfg.chains.iter_mut().for_each(|c| c.rpcs = vec![format!("http://{rpc}/")]);
    cfg.web.basic_auth = Some("op:s3cret".into());
    let db = Db::open_in_memory_shared(&format!("auth{}", N.fetch_add(1, Ordering::Relaxed))).unwrap();
    let addr = serve(Arc::new(App::new(cfg, db, SourceKind::None).unwrap())).await;
    assert_eq!(get(addr, "/", None).await.0, 401);
    assert_eq!(get(addr, "/", Some("op:wrong")).await.0, 401);
    assert_eq!(get(addr, "/", Some("op:s3cret")).await.0, 200);
    assert_eq!(get(addr, "/healthz", None).await.0, 200);
}
