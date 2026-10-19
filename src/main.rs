//! rbm – read-only dashboard for RAILGUN broadcaster operators.

use std::{io::BufRead, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use railgun_broadcaster_monitor::{
    app::App, config, config::SourceKind, db, enrich, ingest, parse, redact, sources, units, waku, web,
};

#[derive(Parser)]
#[command(name = "rbm", version, about = "Read-only dashboard for RAILGUN broadcaster operators")]
struct Cli {
    /// Path to the dashboard config (TOML). Never point this at the broadcaster's config.yaml.
    #[arg(short, long, global = true, env = "RBM_CONFIG", default_value = "rbm.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run collector, enrichment, Waku listener and web UI.
    Run {
        /// Override `source.kind` from the config.
        #[arg(long, value_enum)]
        source: Option<SourceKind>,
    },
    /// Like `run`, reading log lines from stdin (`docker logs -f … 2>&1 | rbm ingest`).
    Ingest,
    /// Parse log files into the database (idempotent) and exit.
    Replay { files: Vec<PathBuf> },
    /// Debug: print parsed+classified lines as JSON (no database).
    Parse { file: Option<PathBuf> },
    /// Validate the config and print a summary.
    CheckConfig,
    /// Listen on Waku for a while and print the fee market (no database writes).
    WakuProbe {
        #[arg(long, default_value_t = 90)]
        secs: u64,
        /// Print all broadcasters for this chain.
        #[arg(long)]
        chain: Option<u64>,
    },
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("RBM_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,waku=error,waku_relay=error,libp2p=error"));
    // No colour codes when stdout is not a terminal (systemd/journald, docker logs).
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_ansi(ansi).init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Parse { file } => return parse_cmd(file),
        Cmd::CheckConfig => {
            let cfg = config::Config::load(&cli.config)?;
            println!("config OK: {}", cli.config.display());
            println!("  broadcaster  {} (gas wallet {})", cfg.broadcaster.name, cfg.broadcaster.gas_wallet);
            println!("  source       {:?}", cfg.source.kind);
            println!(
                "  web          {} (auth: {})",
                cfg.web.bind,
                if cfg.web.basic_auth.is_some() { "basic" } else { "none" }
            );
            println!("  database     {}", cfg.storage.path.display());
            for c in &cfg.chains {
                println!(
                    "  chain {:>6}  {:<10} {} rpc(s), {} token(s), price feed: {}",
                    c.id,
                    c.name,
                    c.rpcs.len(),
                    c.tokens.len(),
                    if c.native_usd_feed.is_some() { "chainlink" } else { "derived" }
                );
            }
            return Ok(());
        }
        _ => {}
    }

    init_tracing();
    let rt =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(4).enable_all().build()?;
    rt.block_on(async move {
        match cli.cmd {
            Cmd::Run { source } => {
                let cfg = config::Config::load(&cli.config)?;
                let kind = source.unwrap_or_else(|| cfg.source.kind.clone());
                run(cfg, kind).await
            }
            Cmd::Ingest => {
                let cfg = config::Config::load(&cli.config)?;
                run(cfg, SourceKind::Stdin).await
            }
            Cmd::Replay { files } => {
                let cfg = config::Config::load(&cli.config)?;
                replay(cfg, files).await
            }
            Cmd::WakuProbe { secs, chain } => {
                let cfg = config::Config::load(&cli.config)?;
                waku_probe(cfg, secs, chain).await
            }
            Cmd::Parse { .. } | Cmd::CheckConfig => unreachable!(),
        }
    })
}

async fn run(cfg: config::Config, kind: SourceKind) -> Result<()> {
    let db = db::Db::open(&cfg.storage.path)?;
    let app = Arc::new(App::new(cfg, db, kind.clone())?);
    tracing::info!(source = ?kind, db = %app.cfg.storage.path.display(), "starting rbm");

    let _source = sources::spawn(app.clone(), kind);
    tokio::spawn(enrich::receipts_loop(app.clone()));
    tokio::spawn(enrich::balances_loop(app.clone()));
    tokio::spawn(enrich::prices_loop(app.clone()));
    tokio::spawn(enrich::maintenance_loop(app.clone()));
    tokio::spawn(enrich::container_loop(app.clone()));
    if app.cfg.waku.enabled {
        tokio::spawn(waku::listener_loop(app.clone()));
        tokio::spawn(waku::sampler_loop(app.clone()));
    }
    if app.cfg.web.enabled {
        let a = app.clone();
        tokio::spawn(async move {
            if let Err(e) = web::serve(a).await {
                tracing::error!(error = %e, "web server failed");
                std::process::exit(1);
            }
        });
    }
    shutdown_signal().await;
    tracing::info!("shutting down");
    // Give the ingest writer a moment to flush its last batch.
    tokio::time::sleep(Duration::from_millis(400)).await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

async fn replay(cfg: config::Config, files: Vec<PathBuf>) -> Result<()> {
    let db = db::Db::open(&cfg.storage.path)?;
    for f in files {
        let file = std::fs::File::open(&f).with_context(|| format!("opening {}", f.display()))?;
        let mut reader = std::io::BufReader::new(file);
        let mut total = ingest::BatchStats::default();
        loop {
            let mut batch = Vec::with_capacity(1000);
            let mut buf = Vec::new();
            while batch.len() < 1000 {
                buf.clear();
                if reader.read_until(b'\n', &mut buf)? == 0 {
                    break;
                }
                batch.push(String::from_utf8_lossy(&buf).into_owned());
            }
            if batch.is_empty() {
                break;
            }
            let stats = db.write(move |c| ingest::ingest_lines(c, &batch)).await?;
            total.new_events += stats.new_events;
            total.duplicates += stats.duplicates;
            total.unparsed += stats.unparsed;
        }
        println!(
            "{}: {} new events, {} duplicates, {} unparsed",
            f.display(),
            total.new_events,
            total.duplicates,
            total.unparsed
        );
    }
    println!("receipts, prices and balances are filled in by `rbm run`.");
    Ok(())
}

fn parse_cmd(file: Option<PathBuf>) -> Result<()> {
    let reader: Box<dyn BufRead> = match file {
        Some(f) => Box::new(std::io::BufReader::new(std::fs::File::open(f)?)),
        None => Box::new(std::io::BufReader::new(std::io::stdin())),
    };
    for line in reader.lines() {
        let line = line?;
        let stripped = parse::strip_ansi(&line);
        let out = match parse::parse_line(&stripped) {
            Some(l) => {
                let (kind, reason) = parse::classify(&l);
                serde_json::json!({
                    "ts": l.ts, "level": l.level, "chain_id": l.chain_id(), "kind": kind.as_str(),
                    "reason": reason, "spans": l.spans.iter().map(|s| &s.name).collect::<Vec<_>>(),
                    "message": redact::redact_text(&l.message),
                    "fields": l.fields.iter()
                        .map(|(k, v)| (k.clone(), serde_json::Value::String(redact::redact_field(k, v))))
                        .collect::<serde_json::Map<_, _>>(),
                })
            }
            None => serde_json::json!({"kind": "unparsed", "text": redact::redact_text(&stripped)}),
        };
        println!("{out}");
    }
    Ok(())
}

async fn waku_probe(cfg: config::Config, secs: u64, chain: Option<u64>) -> Result<()> {
    let db = db::Db::open_in_memory_shared("waku-probe")?;
    let app = Arc::new(App::new(cfg, db, SourceKind::None)?);
    tokio::spawn(waku::listener_loop(app.clone()));
    eprintln!("listening on Waku for {secs} s …");
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let m = app.market.read().unwrap();
        let n: usize = m.offers.values().map(|v| v.len()).sum();
        eprintln!(
            "  {:>3} s: {} peers, {} messages ({} invalid), {} broadcaster/chain offers{}",
            start.elapsed().as_secs(),
            m.status.connected_peers,
            m.status.messages,
            m.status.invalid,
            n,
            m.status.error.as_ref().map(|e| format!(", error: {e}")).unwrap_or_default()
        );
    }
    let views = waku::all_views(&app, &waku::MarketInputs::default());
    println!(
        "{:<10} {:<8} {:>4} {:>8} {:>8} {:>15} {:>6} {:>8} {:>9}",
        "chain", "token", "n", "eligible", "our rank", "us", "picked", "to pick", "in pool"
    );
    for v in &views {
        println!(
            "{:<10} {:<8} {:>4} {:>8} {:>8} {:>15} {:>6} {:>8} {:>9}",
            app.cfg.chain_name(Some(v.chain_id as i64)),
            v.symbol,
            v.n_broadcasters,
            v.n_eligible,
            v.our_rank.map(|r| r.to_string()).unwrap_or("-".into()),
            v.our_eligibility.map(|e| e.label()).unwrap_or("-"),
            v.picked.map(|b| if b { "yes" } else { "no" }).unwrap_or("-"),
            v.change_for_pick.map(|x| format!("{:+.1} %", x * 100.0)).unwrap_or("-".into()),
            v.in_pool
                .map(|b| if b { format!("yes ({})", v.pool_size) } else { format!("no ({})", v.pool_size) })
                .unwrap_or("-".into())
        );
    }
    if let Some(chain) = chain {
        for v in views.iter().filter(|v| v.chain_id == chain) {
            println!("\n{} {} – {} broadcasters", app.cfg.chain_name(Some(chain as i64)), v.symbol, v.n_broadcasters);
            for e in &v.entries {
                let rate = units::dec_to_f64(&e.rate, v.decimals).unwrap_or(f64::NAN);
                println!(
                    "  #{:<3} {}{}{} rate {:<14} {:.3}× pick  {:<14} pool:{}  rel:{}  v{}",
                    e.rank,
                    web_short(&e.address),
                    if e.is_us { " (us)" } else { "" },
                    if e.trusted { " (trusted)" } else { "" },
                    units::fmt_sig(rate, 6),
                    e.ratio_to_cheapest,
                    if e.is_pick { "PICK" } else { e.eligibility.label() },
                    if e.in_pool { "in " } else { "out" },
                    e.reliability.map(|r| format!("{r:.2}")).unwrap_or("-".into()),
                    e.version.clone().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

fn web_short(s: &str) -> String {
    if s.len() > 16 { format!("{}…{}", &s[..10], &s[s.len() - 6..]) } else { s.to_string() }
}
