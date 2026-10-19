//! HTML views. Each page renders `main#content`; htmx re-fetches it on SSE ticks.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde_json::json;

use super::{
    fmt::{self, Status, pill},
    queries::{self, DAY_US},
};
use crate::{app::App, db::now_us, market::Eligibility, parse, waku};

const PAGE_SIZE: i64 = 50;

fn param<'a>(q: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    q.get(k).map(String::as_str).filter(|v| !v.is_empty())
}

fn chain_param(q: &HashMap<String, String>) -> Option<i64> {
    param(q, "chain").and_then(|v| v.parse().ok())
}

fn range_param(q: &HashMap<String, String>, default: &'static str) -> (&'static str, i64) {
    let r = match param(q, "range") {
        Some("24h") => "24h",
        Some("7d") => "7d",
        Some("30d") => "30d",
        Some("90d") => "90d",
        Some("all") => "all",
        _ => default,
    };
    let since = match r {
        "24h" => now_us() - DAY_US,
        "7d" => now_us() - 7 * DAY_US,
        "30d" => now_us() - 30 * DAY_US,
        "90d" => now_us() - 90 * DAY_US,
        _ => 0,
    };
    (r, since)
}

fn page_param(q: &HashMap<String, String>) -> i64 {
    param(q, "page").and_then(|v| v.parse().ok()).unwrap_or(0).max(0)
}

fn qs(pairs: &[(&str, String)]) -> String {
    let s: Vec<String> = pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", v.replace('&', "%26").replace(' ', "+")))
        .collect();
    if s.is_empty() { String::new() } else { format!("?{}", s.join("&")) }
}

const NAV: &[(&str, &str)] = &[
    ("/", "Overview"),
    ("/relays", "Relays"),
    ("/requests", "Requests"),
    ("/profit", "Profit"),
    ("/market", "Market"),
    ("/events", "Events"),
];

/// Full page or just `main#content` for htmx refreshes. `topics` are the SSE events
/// that should refresh this page.
fn layout(app: &App, title: &str, active: &str, url: &str, topics: &[&str], partial: bool, content: Markup) -> Markup {
    let trigger =
        topics.iter().map(|t| format!("sse:{t}")).chain(["every 60s".to_string()]).collect::<Vec<_>>().join(", ");
    let main = html! {
        main #content hx-get=(url) hx-trigger=(trigger) hx-swap="outerHTML" hx-select="#content" {
            (content)
        }
    };
    if partial {
        return main;
    }
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="htmx-config" content=r#"{"includeIndicatorStyles":false,"allowEval":false,"allowScriptTags":false}"#;
                title { (title) " · " (app.cfg.broadcaster.name) " · rbm" }
                link rel="icon" href="/assets/icon.svg";
                link rel="stylesheet" href="/assets/vendor/uPlot.min.css";
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/vendor/htmx.min.js" defer {}
                script src="/assets/vendor/htmx-ext-sse.js" defer {}
                script src="/assets/vendor/uPlot.iife.min.js" defer {}
                script src="/assets/app.js" defer {}
            }
            body hx-ext="sse" sse-connect="/sse" {
                header.top {
                    a.brand href="/" { img src="/assets/icon.svg" alt="" width="24" height="24"; span { "Broadcaster monitor" } }
                    nav {
                        @for (href, label) in NAV {
                            a href=(href) class=[(*href == active).then_some("active")] { (label) }
                        }
                    }
                    span.who title=(app.cfg.broadcaster.railgun_address) { (app.cfg.broadcaster.name) }
                    span #live.live title="Live updates (server-sent events)" { "connecting…" }
                }
                (main)
                footer {
                    "railgun-broadcaster-monitor " (env!("CARGO_PKG_VERSION"))
                    " · parser v" (parse::PARSER_VERSION) " (" (parse::PARSER_TARGET) ")"
                    " · read-only · times in UTC"
                }
            }
        }
    }
}

fn chain_options(app: &App, selected: Option<i64>) -> Markup {
    html! {
        option value="" selected[selected.is_none()] { "All chains" }
        @for c in &app.cfg.chains {
            option value=(c.id) selected[selected == Some(c.id as i64)] { (c.name) }
        }
    }
}

fn range_options(selected: &str, choices: &[&str]) -> Markup {
    html! {
        @for r in choices {
            option value=(r) selected[*r == selected] {
                (match *r { "24h" => "Last 24 hours", "7d" => "Last 7 days", "30d" => "Last 30 days", "90d" => "Last 90 days", _ => "All time" })
            }
        }
    }
}

fn pager(base: &str, extra: &[(&str, String)], page: i64, total: i64) -> Markup {
    let pages = (total + PAGE_SIZE - 1) / PAGE_SIZE;
    let link = |p: i64| {
        let mut v: Vec<(&str, String)> = extra.to_vec();
        v.push(("page", p.to_string()));
        format!("{base}{}", qs(&v))
    };
    html! {
        @if pages > 1 {
            nav.pager {
                @if page > 0 { a href=(link(page - 1)) { "← Newer" } }
                span { "Page " (page + 1) " of " (pages) " · " (total) " rows" }
                @if page + 1 < pages { a href=(link(page + 1)) { "Older →" } }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Overview

pub async fn overview(app: &App, partial: bool) -> Result<Markup> {
    let now = now_us();
    let chains: Vec<i64> = app.cfg.chains.iter().map(|c| c.id as i64).collect();
    let (totals, healths, recent, inputs) = app
        .db
        .read(move |c| {
            let t = queries::totals(c, now)?;
            let hs = chains.iter().map(|id| queries::chain_health(c, *id, now)).collect::<Result<Vec<_>>>()?;
            let (recent, _) = queries::requests(c, None, queries::outcome_filter("all"), 0, 8, 0)?;
            let inputs = waku::market_inputs(c)?;
            Ok((t, hs, recent, inputs))
        })
        .await?;
    let views = waku::all_views(app, &inputs);
    // When was our own fee offer last seen on Waku, per chain?
    let our_seen: HashMap<u64, i64> = {
        let m = app.market.read().unwrap();
        m.offers.iter().filter_map(|(chain, by)| by.get(&m.our_address).map(|o| (*chain, o.seen_us))).collect()
    };
    let wstatus = {
        let m = app.market.read().unwrap();
        (
            m.status.enabled,
            m.status.connected_peers,
            m.status.messages,
            m.status.error.clone(),
            m.status.last_message_us,
        )
    };
    let cfg = &app.cfg;
    let h = &cfg.health;

    let log_age = totals.last_ingest_wall.map(|t| now - t);
    let content = html! {
        h1 { "Overview" }
        section.tiles {
            div.tile { div.label { "Relays, last 24 h" } div.value { (totals.relays_24h) } div.sub { (totals.relays_7d) " in 7 days" } }
            div.tile {
                div.label { "Profit, last 7 days" }
                div.value { (fmt::signed(fmt::usd(totals.profit_7d_usd), totals.profit_7d_usd)) }
                @if totals.profit_7d_unpriced > 0 { div.sub { (totals.profit_7d_unpriced) " relay(s) without fee or price data" } }
            }
            div.tile {
                div.label { "Rejected / failed, last 24 h" }
                div.value { (totals.problems_24h) }
                div.sub { a href="/requests" { "Show reasons" } }
            }
            div.tile {
                div.label { "Log ingest" }
                @let (log_st, log_label, log_hint) = log_status(&app.source, log_age, (now - app.started_us) / 1_000_000);
                div.value { (pill(log_st, log_label)) }
                div.sub {
                    @if let Some(t) = totals.last_log_ts { "Last line " (fmt::when(t, now)) } @else { "No log lines ingested" }
                    " · " (totals.unparsed_24h + totals.other_24h) " unrecognised in 24 h"
                }
                @if let Some(h) = log_hint { div.sub { (h) } }
            }
            div.tile {
                div.label { "Waku fee market" }
                div.value {
                    @if !wstatus.0 { (pill(Status::Unknown, "Disabled")) }
                    @else if wstatus.3.is_some() { (pill(Status::Critical, "Error")) }
                    @else if wstatus.1 > 0 { (pill(Status::Good, &format!("{} peers", wstatus.1))) }
                    @else { (pill(Status::Warning, "Connecting")) }
                }
                div.sub {
                    (wstatus.2) " fee messages"
                    @if let Some(t) = wstatus.4 { " · last " (fmt::when(t, now)) }
                }
            }
        }

        section.chains {
            @for hc in &healths {
                @let chain = cfg.chain(hc.chain as u64).unwrap();
                @let pub_age = hc.last_fee_publish.map(|t| (now - t) / 1_000_000);
                @let chain_views: Vec<_> = views.iter().filter(|v| v.chain_id == hc.chain as u64 && v.our_rate.is_some()).collect();
                @let picked = chain_views.iter().filter(|v| v.picked == Some(true)).count();
                @let in_band = chain_views.iter().filter(|v| v.our_eligibility == Some(Eligibility::Eligible)).count();
                @let runway = runway(hc);
                article.card {
                    header {
                        h2 { (chain.name) }
                        @let (st, label) = publish_status(pub_age, h);
                        (pill(st, label))
                    }
                    dl {
                        dt { "Last fee publish" }
                        dd { @if let Some(t) = hc.last_fee_publish { (fmt::when(t, now)) } @else { "–" } }
                        dt { "Our offer on Waku" }
                        dd {
                            @match our_seen.get(&(hc.chain as u64)) {
                                Some(t) => { "seen " (fmt::when(*t, now)) }
                                None if wstatus.0 => span.muted { "not seen" },
                                None => span.muted { "Waku disabled" },
                            }
                        }
                        dt { "Gas balance" }
                        dd {
                            (fmt::native(cfg, hc.chain, hc.balance))
                            @if let (Some(b), Some(p)) = (hc.balance, hc.price_usd) { span.muted { " · " (fmt::usd(Some(b * p))) } }
                            @if let (Some(b), Some(low)) = (hc.balance, chain.low_balance) {
                                @if b < low { " " (pill(Status::Warning, "Low")) }
                            }
                        }
                        dt { "Runway" }
                        dd {
                            @if let Some((n, basis)) = &runway {
                                "≈ " (format!("{n:.0}")) " relays " span.muted title=(basis) { "(" (basis) ")" }
                            } @else { "–" }
                        }
                        dt { "Gas price" }
                        dd { @if let Some(g) = hc.gas_price_wei { (crate::units::fmt_sig(g / 1e9, 3)) " gwei" } @else { "–" } }
                        dt { "Relays, 7 days" }
                        dd {
                            (hc.relays_7d)
                            @if hc.relays_7d > 0 {
                                " · " (fmt::signed(fmt::native(cfg, hc.chain, hc.profit_7d_native), hc.profit_7d_native))
                            }
                        }
                        dt { "Rejected / failed, 7 days" }
                        dd { @if hc.problems_7d > 0 { a href=(format!("/requests?chain={}&range=7d", hc.chain)) { (hc.problems_7d) } } @else { "0" } }
                        dt { span title="Railway and Terminal Wallet pick the cheapest offer within the trusted-fee band" { "Wallet pick" } }
                        dd {
                            @if chain_views.is_empty() { span.muted { "no data" } }
                            @else {
                                a href=(format!("/market#chain-{}", hc.chain)) { (picked) " of " (chain_views.len()) " tokens" }
                                span.muted { " · " (in_band) " in band" }
                            }
                        }
                        dt { "RPC trouble, 1 h / 24 h" }
                        dd {
                            @let s1 = hc.counts_1h.get("sync_warn").copied().unwrap_or(0) + hc.counts_1h.get("price_warn").copied().unwrap_or(0);
                            @let s24 = hc.counts_24h.get("sync_warn").copied().unwrap_or(0) + hc.counts_24h.get("price_warn").copied().unwrap_or(0);
                            span title="sync_live / sync_head / failed to get prices warnings" { (s1) " / " (s24) " warnings" }
                            @let e24 = hc.counts_24h.get("fee_publish_error").copied().unwrap_or(0) + hc.counts_24h.get("update_prices_failed").copied().unwrap_or(0);
                            @if e24 > 0 { " · " (pill(Status::Warning, &format!("{e24} publish/price errors"))) }
                        }
                        dt { "Last (re)start" }
                        dd { @if let Some(t) = hc.last_start { (fmt::when(t, now)) } @else { "–" } }
                        dt { "Wallets / version" }
                        dd { (hc.available_wallets.map(|v| v.to_string()).unwrap_or("–".into())) " / " (hc.version.clone().unwrap_or("–".into())) }
                    }
                }
            }
        }

        section.split {
            article.card {
                header { h2 { "Container" } }
                @if let Some(c) = &totals.container {
                    @if let Some(err) = c.get("error").and_then(|v| v.as_str()) {
                        p { (pill(Status::Warning, "Inspect failed")) " " span.muted { (err) } }
                    } @else {
                        @let status = c.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                        @let health = c.get("health").and_then(|v| v.as_str());
                        dl {
                            dt { "State" }
                            dd {
                                @if status == "running" { (pill(Status::Good, "Running")) } @else { (pill(Status::Critical, status)) }
                                @if let Some(hl) = health {
                                    " " @if hl == "healthy" { (pill(Status::Good, "Healthy")) } @else { (pill(Status::Warning, hl)) }
                                }
                            }
                            dt { "Started" } dd { (c.get("started_at").and_then(|v| v.as_str()).unwrap_or("–")) }
                            dt { "Restart count" } dd { (c.get("restart_count").map(|v| v.to_string()).unwrap_or("–".into())) }
                            dt { "Image" } dd.mono { (c.get("image").and_then(|v| v.as_str()).unwrap_or("–")) }
                        }
                    }
                } @else {
                    p.muted { "Container inspection is off (it needs Docker access, which is root-equivalent). Restarts are detected from the broadcaster's startup log lines." }
                }
                p {
                    "Last restart seen in logs: "
                    @if let Some((t, chains)) = &totals.last_restart { (fmt::when(*t, now)) span.muted { " (chains " (chains) ")" } } @else { "–" }
                }
            }
            article.card {
                header { h2 { "Recent requests" } a href="/requests?outcome=all" { "All" } }
                (requests_table(app, &recent, now, true))
            }
        }
    };
    Ok(layout(app, "Overview", "/", "/", &["health", "relays", "requests", "fees", "market"], partial, content))
}

/// Log-ingest tile: state, label and, when something looks wrong, what to check.
/// `age_us` is the age of the newest ingested line, `uptime_secs` how long `rbm` has been running.
pub fn log_status(
    source: &crate::config::SourceKind,
    age_us: Option<i64>,
    uptime_secs: i64,
) -> (Status, &'static str, Option<&'static str>) {
    use crate::config::SourceKind::*;
    // The broadcaster logs every minute per chain, so five quiet minutes is already suspicious.
    const QUIET_US: i64 = 5 * 60 * 1_000_000;
    let hint = match source {
        Journald => {
            "Is the broadcaster container using the journald logging driver, and does source.journald_match name it?"
        }
        Command => "Check the container name in source.command and that the service can run it (docker group).",
        Stdin => "Is `docker logs -f … | rbm ingest` still piping into this process?",
        None => "",
    };
    match (source, age_us) {
        (None, _) => (Status::Unknown, "No live source", Option::None),
        (_, Some(a)) if a < QUIET_US => (Status::Good, "Receiving", Option::None),
        (_, Some(_)) => (Status::Warning, "Quiet", Some(hint)),
        (_, Option::None) if uptime_secs < 120 => (Status::Unknown, "No lines yet", Option::None),
        (_, Option::None) => (Status::Warning, "No lines received", Some(hint)),
    }
}

/// Fee-publish freshness: green < warn (3 min), amber < stale (10 min), red after.
pub fn publish_status(age_secs: Option<i64>, h: &crate::config::Health) -> (Status, &'static str) {
    match age_secs {
        None => (Status::Unknown, "No fee publish seen"),
        Some(a) if a < h.fee_publish_warn_secs => (Status::Good, "Publishing fees"),
        Some(a) if a < h.fee_publish_stale_secs => (Status::Warning, "Fee publish late"),
        Some(_) => (Status::Critical, "Fee publish stale"),
    }
}

/// Balance ÷ average actual relay cost (falls back to the last estimate, then gas price × 1.3M gas).
fn runway(h: &queries::ChainHealth) -> Option<(f64, String)> {
    let b = h.balance?;
    let (cost, basis) = if let Some((avg, n)) = h.avg_cost.filter(|(a, _)| *a > 0.0) {
        (avg, format!("avg actual cost of last {n} relay{}", if n == 1 { "" } else { "s" }))
    } else if let Some(est) = h.last_est_cost.filter(|v| *v > 0.0) {
        (est, "last logged gas estimate".to_string())
    } else {
        let gp = h.gas_price_wei.filter(|v| *v > 0.0)?;
        (gp * 1.3e6 / 1e18, "current gas price × 1.3M gas".to_string())
    };
    Some((b / cost, basis))
}

fn outcome_pill(outcome: &str) -> Markup {
    match outcome {
        "submitted" => pill(Status::Good, "Submitted"),
        "pending" => pill(Status::Unknown, "Pending"),
        "rejected" => pill(Status::Warning, "Rejected"),
        "failed" => pill(Status::Critical, "Failed"),
        _ => pill(Status::Unknown, "Unknown"),
    }
}

fn requests_table(app: &App, rows: &[queries::RequestRow], now: i64, compact: bool) -> Markup {
    let cfg = &app.cfg;
    html! {
        @if rows.is_empty() {
            p.muted { "No requests in this range." }
        } @else {
            div.tablewrap { table {
                thead { tr {
                    th { "Time" } th { "Chain" } th { "Fee" } th { "Outcome" } th { "Reason" }
                    @if !compact { th { "Detail" } th { "Tx" } }
                } }
                tbody {
                    @for r in rows {
                        tr {
                            td { a href=(format!("/requests/{}", r.id)) { (fmt::when(r.ts, now)) } }
                            td { (cfg.chain_name(r.chain)) }
                            td.num {
                                (fmt::token_amount(cfg, r.chain, r.fee_token.as_deref(), r.fee_amount.as_deref()))
                                " " (fmt::symbol(cfg, r.chain, r.fee_token.as_deref()))
                            }
                            td { (outcome_pill(&r.outcome)) }
                            td.mono { (r.reason.clone().unwrap_or_default()) }
                            @if !compact {
                                td.detail title=(r.detail.clone().unwrap_or_default()) {
                                    (r.detail.as_deref().map(|d| d.chars().take(90).collect::<String>()).unwrap_or_default())
                                }
                                td { @if let (Some(h), Some(c)) = (&r.tx_hash, r.chain) { (fmt::tx_link(cfg, c, h)) } }
                            }
                        }
                    }
                }
            } }
        }
    }
}

// ---------------------------------------------------------------------------
// Relays

pub async fn relays(app: &App, q: &HashMap<String, String>, partial: bool) -> Result<Markup> {
    let now = now_us();
    let chain = chain_param(q);
    let (range, since) = range_param(q, "30d");
    let page = page_param(q);
    let (rows, total) = app.db.read(move |c| queries::relays(c, chain, since, PAGE_SIZE, page * PAGE_SIZE)).await?;
    let cfg = &app.cfg;
    let filters = vec![("chain", chain.map(|c| c.to_string()).unwrap_or_default()), ("range", range.to_string())];
    let url = format!(
        "/relays{}",
        qs(&{
            let mut f = filters.clone();
            if page > 0 {
                f.push(("page", page.to_string()))
            }
            f
        })
    );
    let content = html! {
        h1 { "Relays" }
        form.filters method="get" {
            label { "Chain " select name="chain" { (chain_options(app, chain)) } }
            label { "Range " select name="range" { (range_options(range, &["24h", "7d", "30d", "90d", "all"])) } }
            button type="submit" { "Apply" }
        }
        p.muted {
            "Fee value = the broadcaster's own conversion of the fee into the gas token (the logged " code { "refund" } "). "
            "Actual cost = gasUsed × effectiveGasPrice from the receipt. Route: tx sent to RelayAdapt → mev queue, otherwise mempool."
        }
        @if rows.is_empty() {
            p.muted { "No relays in this range." }
        } @else {
            div.tablewrap { table {
                thead { tr {
                    th { "Time" } th { "Chain" } th { "Tx" } th { "Fee" } th.num { "Fee value" } th.num { "Fee USD" }
                    th.num { "Est. cost" } th.num { "Actual cost" } th.num { "Profit" } th.num { "Margin" } th.num { "Profit USD" }
                    th { "Route" } th { "Receipt" } th { "Fee-note PPOI" }
                } }
                tbody {
                    @for r in &rows {
                        tr {
                            td {
                                @if let Some(id) = r.request_id { a href=(format!("/requests/{id}")) { (fmt::when(r.ts, now)) } }
                                @else { (fmt::when(r.ts, now)) }
                            }
                            td { (cfg.chain_name(Some(r.chain))) }
                            td {
                                (fmt::tx_link(cfg, r.chain, &r.tx_hash))
                                @if r.source != "log" { " " span.muted title="Seen only via fee-note assurance lines; the request itself predates the dashboard" { "(" (r.source) ")" } }
                            }
                            td.num {
                                @if r.fee_amount.is_some() {
                                    (fmt::token_amount(cfg, Some(r.chain), r.fee_token.as_deref(), r.fee_amount.as_deref()))
                                    " " (fmt::symbol(cfg, Some(r.chain), r.fee_token.as_deref()))
                                } @else { span.muted title="Relay seen only via fee-note/backfill; fee amount unknown" { "unknown" } }
                            }
                            td.num title=(r.fee_native_source.clone().unwrap_or_default()) { (fmt::native(cfg, r.chain, r.fee_native)) }
                            td.num { (fmt::usd(r.fee_usd)) }
                            td.num { (fmt::native(cfg, r.chain, r.est_cost)) }
                            td.num title=(fmt::usd(r.cost_usd)) { (fmt::native(cfg, r.chain, r.cost_native)) }
                            td.num { (fmt::signed(fmt::native(cfg, r.chain, r.profit_native), r.profit_native)) }
                            td.num { (fmt::signed(fmt::pct(r.margin()), r.margin())) }
                            td.num { (fmt::signed(fmt::usd(r.profit_usd), r.profit_usd)) }
                            td { (r.route.clone().unwrap_or("–".into())) }
                            td {
                                @match (r.receipt_state.as_str(), r.status) {
                                    ("confirmed", Some(1)) => (pill(Status::Good, "Success")),
                                    ("confirmed", _) => (pill(Status::Critical, "Reverted")),
                                    ("not_found", _) => (pill(Status::Warning, "Not found")),
                                    _ => (pill(Status::Unknown, "Pending")),
                                }
                            }
                            td title=(r.assurance_detail.clone().unwrap_or_default()) {
                                @match r.assurance.as_deref() {
                                    Some("completed") => (pill(Status::Good, "Completed")),
                                    Some("submitted") => (pill(Status::Unknown, "Submitted")),
                                    Some("failed") => (pill(Status::Critical, "Failed")),
                                    Some("pending") => (pill(Status::Unknown, "Pending")),
                                    _ => span.muted { "–" },
                                }
                            }
                        }
                    }
                }
            } }
            (pager("/relays", &filters, page, total))
        }
    };
    Ok(layout(app, "Relays", "/relays", &url, &["relays"], partial, content))
}

// ---------------------------------------------------------------------------
// Requests / rejections

pub async fn requests(app: &App, q: &HashMap<String, String>, partial: bool) -> Result<Markup> {
    let now = now_us();
    let chain = chain_param(q);
    let (range, since) = range_param(q, "7d");
    let outcome = match param(q, "outcome") {
        Some(o @ ("all" | "submitted" | "pending")) => o.to_string(),
        _ => "problems".to_string(),
    };
    let page = page_param(q);
    let o2 = outcome.clone();
    let (rows, total, groups, by_day, counts) = app
        .db
        .read(move |c| {
            let (rows, total) =
                queries::requests(c, chain, queries::outcome_filter(&o2), since, PAGE_SIZE, page * PAGE_SIZE)?;
            let groups = queries::reason_groups(c, since, chain)?;
            let by_day = queries::reason_by_day(c, since.max(now - 14 * DAY_US), chain)?;
            let counts = queries::outcome_counts(c, since, chain)?;
            Ok((rows, total, groups, by_day, counts))
        })
        .await?;
    let cfg = &app.cfg;
    let filters = vec![
        ("chain", chain.map(|c| c.to_string()).unwrap_or_default()),
        ("range", range.to_string()),
        ("outcome", outcome.clone()),
    ];
    let url = format!("/requests{}", qs(&filters));
    let total_req: i64 = counts.values().sum();
    // reason × day matrix
    let mut days: Vec<i64> = by_day.iter().map(|(d, _, _)| *d).collect();
    days.dedup();
    let mut reasons: BTreeMap<String, HashMap<i64, i64>> = BTreeMap::new();
    for (d, r, n) in &by_day {
        reasons.entry(r.clone()).or_default().insert(*d, *n);
    }
    let content = html! {
        h1 { "Requests" }
        form.filters method="get" {
            label { "Chain " select name="chain" { (chain_options(app, chain)) } }
            label { "Range " select name="range" { (range_options(range, &["24h", "7d", "30d", "90d", "all"])) } }
            label { "Outcome "
                select name="outcome" {
                    option value="problems" selected[outcome == "problems"] { "Rejected / failed / unknown" }
                    option value="all" selected[outcome == "all"] { "All" }
                    option value="submitted" selected[outcome == "submitted"] { "Submitted" }
                    option value="pending" selected[outcome == "pending"] { "Pending" }
                }
            }
            button type="submit" { "Apply" }
        }
        section.tiles {
            div.tile { div.label { "Requests" } div.value { (total_req) } }
            div.tile { div.label { "Submitted" } div.value { (counts.get("submitted").copied().unwrap_or(0)) } }
            div.tile { div.label { "Rejected" } div.value { (counts.get("rejected").copied().unwrap_or(0)) } div.sub { "validation / policy (POI, decrypt, gas too high)" } }
            div.tile { div.label { "Failed" } div.value { (counts.get("failed").copied().unwrap_or(0)) } div.sub { "operational (RPC, estimate, submit)" } }
            div.tile { div.label { "No outcome" } div.value { (counts.get("unknown").copied().unwrap_or(0)) } div.sub { "no result line within 10 min" } }
        }
        section.split {
            article.card {
                header { h2 { "By reason" } }
                @if groups.is_empty() { p.muted { "No rejected or failed requests in this range." } }
                @else {
                    div.tablewrap { table {
                        thead { tr { th { "Reason" } th { "Chain" } th { "Outcome" } th.num { "Count" } th { "First" } th { "Last" } } }
                        tbody {
                            @for g in &groups {
                                tr {
                                    td.mono { (g.reason) }
                                    td { (cfg.chain_name(g.chain)) }
                                    td { (outcome_pill(&g.outcome)) }
                                    td.num { (g.count) }
                                    td { (fmt::when(g.first, now)) }
                                    td { (fmt::when(g.last, now)) }
                                }
                            }
                        }
                    } }
                }
            }
            article.card {
                header { h2 { "Per day (UTC), last 14 days" } }
                @if reasons.is_empty() { p.muted { "Nothing to show." } }
                @else {
                    div.tablewrap { table {
                        thead { tr { th { "Reason" } @for d in &days { th.num { (fmt::day(*d).get(5..).unwrap_or_default()) } } } }
                        tbody {
                            @for (r, m) in &reasons {
                                tr {
                                    td.mono { (r) }
                                    @for d in &days { td.num { @if let Some(n) = m.get(d) { (n) } @else { span.muted { "·" } } } }
                                }
                            }
                        }
                    } }
                }
            }
        }
        h2 { "Requests" }
        (requests_table(app, &rows, now, false))
        (pager("/requests", &filters, page, total))
    };
    Ok(layout(app, "Requests", "/requests", &url, &["requests"], partial, content))
}

pub async fn request_detail(app: &App, id: i64) -> Result<Markup> {
    let now = now_us();
    let (req, events) =
        app.db.read(move |c| Ok((queries::request(c, id)?, queries::events(c, None, None, Some(id), 100)?))).await?;
    let cfg = &app.cfg;
    let content = html! {
        h1 { "Request #" (id) }
        @if let Some(r) = req {
            dl.wide {
                dt { "Time" } dd { (fmt::ts(r.ts)) " UTC (" (fmt::ago(r.ts, now)) ")" }
                dt { "Chain" } dd { (cfg.chain_name(r.chain)) }
                dt { "Fee" } dd {
                    (fmt::token_amount(cfg, r.chain, r.fee_token.as_deref(), r.fee_amount.as_deref())) " "
                    (fmt::symbol(cfg, r.chain, r.fee_token.as_deref()))
                    @if let Some(t) = &r.fee_token { span.muted.mono { " " (t) } }
                }
                dt { "Outcome" } dd { (outcome_pill(&r.outcome)) " " span.mono { (r.reason.clone().unwrap_or_default()) } }
                dt { "Stage reached" } dd { (["–", "parsed", "converting", "gas estimated", "broadcast", "submitted"].get(r.stage as usize).unwrap_or(&"?")) }
                @if let Some(d) = &r.detail { dt { "Detail" } dd.mono { (d) } }
                @if let Some(c) = r.chain {
                    dt { "Estimated cost" } dd { (fmt::native(cfg, c, r.est_cost)) }
                    dt { "Fee value (refund)" } dd { (fmt::native(cfg, c, r.refund)) }
                }
                @if let Some(rc) = &r.rpc_clients { dt { "Broadcast via" } dd.mono { (rc) } }
                @if let (Some(h), Some(c)) = (&r.tx_hash, r.chain) { dt { "Tx" } dd { (fmt::tx_link(cfg, c, h)) " · " a href="/relays" { "relays" } } }
            }
            h2 { "Correlated log lines" }
            (events_table(app, &events, now))
        } @else {
            p { "Not found." }
        }
    };
    Ok(layout(app, "Request", "/requests", &format!("/requests/{id}"), &[], false, content))
}

// ---------------------------------------------------------------------------
// Profit

const CHAIN_SLOTS: usize = 4;

pub async fn profit(app: &App, q: &HashMap<String, String>, partial: bool) -> Result<Markup> {
    let (range, since) = range_param(q, "30d");
    let weekly = param(q, "bucket") == Some("week");
    let (daily, tokens) = app.db.read(move |c| Ok((queries::daily(c, since)?, queries::by_token(c, since)?))).await?;
    let cfg = &app.cfg;

    // Buckets (day index, or Monday-aligned week start) × chain.
    let bucket_of = |d: i64| if weekly { d - (d + 3).rem_euclid(7) } else { d };
    let mut buckets: BTreeMap<i64, HashMap<i64, queries::Agg>> = BTreeMap::new();
    let mut per_chain: BTreeMap<i64, queries::Agg> = BTreeMap::new();
    for (d, chain, a) in &daily {
        let e = buckets.entry(bucket_of(*d)).or_default().entry(*chain).or_default();
        add(e, a);
        add(per_chain.entry(*chain).or_default(), a);
    }
    // Fill empty buckets over the whole range (up to today) so the time axis is continuous.
    if let Some(&first_data) = buckets.keys().next() {
        let step = if weekly { 7 } else { 1 };
        let first = if since > 0 { bucket_of(since / DAY_US) } else { first_data };
        let last = bucket_of(now_us() / DAY_US);
        let mut d = first.min(first_data);
        while d <= last {
            buckets.entry(d).or_default();
            d += step;
        }
    }
    let mut total = queries::Agg::default();
    for a in per_chain.values() {
        add(&mut total, a);
    }

    let xs: Vec<i64> = buckets.keys().map(|d| d * 86_400).collect();
    let profit_usd: Vec<f64> = buckets.values().map(|m| m.values().map(|a| a.profit_usd).sum()).collect();
    let counts: Vec<i64> = buckets.values().map(|m| m.values().map(|a| a.relays).sum()).collect();
    let chains: Vec<i64> = cfg.chains.iter().map(|c| c.id as i64).collect();
    let cumulative: Vec<serde_json::Value> = chains
        .iter()
        .take(CHAIN_SLOTS)
        .enumerate()
        .map(|(i, cid)| {
            let mut acc = 0.0;
            let ys: Vec<f64> = buckets
                .values()
                .map(|m| {
                    acc += m.get(cid).map_or(0.0, |a| a.profit_usd);
                    acc
                })
                .collect();
            json!({"label": cfg.chain_name(Some(*cid)), "slot": i + 1, "values": ys})
        })
        .collect();
    let bucket_label = if weekly { "week" } else { "day" };
    let charts = json!({
        "x": xs,
        "profit": {"label": format!("Net profit per {bucket_label} (USD)"), "values": profit_usd},
        "count": {"label": format!("Relays per {bucket_label}"), "values": counts},
        "cumulative": cumulative,
    });

    let filters = vec![("range", range.to_string()), ("bucket", if weekly { "week".into() } else { String::new() })];
    let url = format!("/profit{}", qs(&filters));
    let margin = |a: &queries::Agg| (a.cost_native > 0.0).then(|| a.profit_native / a.cost_native);
    let content = html! {
        h1 { "Profit" }
        form.filters method="get" {
            label { "Range " select name="range" { (range_options(range, &["7d", "30d", "90d", "all"])) } }
            label { "Bucket "
                select name="bucket" {
                    option value="" selected[!weekly] { "Day" }
                    option value="week" selected[weekly] { "Week" }
                }
            }
            button type="submit" { "Apply" }
        }
        section.tiles {
            div.tile { div.label { "Net profit" } div.value { (fmt::signed(fmt::usd(Some(total.profit_usd)), Some(total.profit_usd))) }
                @if total.unpriced > 0 { div.sub { (total.unpriced) " relay(s) not included (fee or USD price unknown)" } } }
            div.tile { div.label { "Relays" } div.value { (total.relays) } }
            div.tile { div.label { "Fees earned" } div.value { (fmt::usd(Some(total.fee_usd))) } }
            div.tile { div.label { "Gas spent" } div.value { (fmt::usd(Some(total.cost_usd))) } }
        }
        @if total.relays == 0 {
            p.muted { "No relays in this range yet." }
        } @else {
            script #chart-data type="application/json" { (PreEscaped(charts.to_string().replace("</", "<\\/"))) }
            section.charts {
                figure.chart { figcaption { (format!("Net profit per {bucket_label} (USD)")) } div.plot data-chart="profit" {} }
                figure.chart { figcaption { (format!("Relays per {bucket_label}")) } div.plot data-chart="count" {} }
                figure.chart.wide { figcaption { "Cumulative profit by chain (USD)" } div.plot data-chart="cumulative" {} }
            }
            details.tableview {
                summary { "Table view of the charts" }
                div.tablewrap { table {
                    thead { tr { th { (if weekly { "Week of" } else { "Day" }) } th.num { "Relays" } th.num { "Net profit USD" } } }
                    tbody {
                        @for (i, x) in xs.iter().enumerate().rev() {
                            tr { td { (fmt::day(x * 1_000_000)) } td.num { (counts[i]) } td.num { (fmt::usd(Some(profit_usd[i]))) } }
                        }
                    }
                } }
            }
            section.split {
                article.card {
                    header { h2 { "By chain" } }
                    div.tablewrap { table {
                        thead { tr { th { "Chain" } th.num { "Relays" } th.num { "Fees" } th.num { "Gas" } th.num { "Profit" } th.num { "Margin" } th.num { "Profit USD" } } }
                        tbody {
                            @for (cid, a) in &per_chain {
                                tr {
                                    td { (cfg.chain_name(Some(*cid))) }
                                    td.num { (a.relays) }
                                    td.num { (fmt::native(cfg, *cid, Some(a.fee_native))) }
                                    td.num { (fmt::native(cfg, *cid, Some(a.cost_native))) }
                                    td.num { (fmt::signed(fmt::native(cfg, *cid, Some(a.profit_native)), Some(a.profit_native))) }
                                    td.num { (fmt::pct(margin(a))) }
                                    td.num { (fmt::signed(fmt::usd(Some(a.profit_usd)), Some(a.profit_usd))) }
                                }
                            }
                        }
                    } }
                    p.muted { "Margin = profit ÷ actual gas cost. Relays with an unknown fee (pre-dashboard) count towards gas only." }
                }
                article.card {
                    header { h2 { "By fee token" } }
                    div.tablewrap { table {
                        thead { tr { th { "Chain" } th { "Token" } th.num { "Relays" } th.num { "Fees (token)" } th.num { "Profit" } th.num { "Profit USD" } } }
                        tbody {
                            @for (cid, tok, a, amount) in &tokens {
                                @let dec = tok.as_deref().and_then(|t| cfg.token(*cid as u64, t)).map(|t| t.decimals);
                                tr {
                                    td { (cfg.chain_name(Some(*cid))) }
                                    td { (fmt::symbol(cfg, Some(*cid), tok.as_deref())) }
                                    td.num { (a.relays) }
                                    td.num { @if let Some(d) = dec { (crate::units::fmt_sig(amount / 10f64.powi(d as i32), 4)) } @else { "–" } }
                                    td.num { (fmt::signed(fmt::native(cfg, *cid, Some(a.profit_native)), Some(a.profit_native))) }
                                    td.num { (fmt::signed(fmt::usd(Some(a.profit_usd)), Some(a.profit_usd))) }
                                }
                            }
                        }
                    } }
                }
            }
        }
    };
    Ok(layout(app, "Profit", "/profit", &url, &["relays"], partial, content))
}

fn add(a: &mut queries::Agg, b: &queries::Agg) {
    a.relays += b.relays;
    a.fee_native += b.fee_native;
    a.cost_native += b.cost_native;
    a.profit_native += b.profit_native;
    a.profit_usd += b.profit_usd;
    a.fee_usd += b.fee_usd;
    a.cost_usd += b.cost_usd;
    a.unknown += b.unknown;
    a.unpriced += b.unpriced;
}

// ---------------------------------------------------------------------------
// Market

fn wallet_status(v: &crate::market::TokenView) -> Markup {
    match (v.picked, v.our_eligibility) {
        (Some(true), _) => pill(Status::Good, "Wallet pick"),
        (_, Some(Eligibility::Eligible)) => pill(Status::Warning, "Eligible, not cheapest"),
        (_, Some(Eligibility::TooExpensive)) => pill(Status::Critical, "Above band"),
        (_, Some(Eligibility::TooCheap)) => pill(Status::Critical, "Below band"),
        (_, Some(Eligibility::NoTrustedFee)) => html! {
            span title="No live offer from a trusted fee signer for this token: wallets show no broadcaster for it. Normal for a few minutes after rbm starts." {
                (pill(Status::Unknown, "No trusted fee"))
            }
        },
        _ => pill(Status::Unknown, "Not offered"),
    }
}

pub async fn market(app: &App, partial: bool) -> Result<Markup> {
    let now = now_us();
    let inputs = app.db.read(waku::market_inputs).await?;
    let views = waku::all_views(app, &inputs);
    let keys: Vec<(i64, String)> = views.iter().map(|v| (v.chain_id as i64, v.token.clone())).collect();
    let shares: HashMap<(i64, String), (f64, i64)> = app
        .db
        .read(move |c| {
            let mut out = HashMap::new();
            for k in keys {
                if let Some(s) = queries::pick_share(c, k.0, &k.1, now - DAY_US)? {
                    out.insert(k, s);
                }
            }
            Ok(out)
        })
        .await?;
    let (status, pool_factor) = {
        let m = app.market.read().unwrap();
        (
            (
                m.status.enabled,
                m.status.connected_peers,
                m.status.messages,
                m.status.invalid,
                m.status.last_message_us,
                m.status.error.clone(),
            ),
            m.pool_factor,
        )
    };
    let cfg = &app.cfg;
    let rate_text = |v: &crate::market::TokenView, rate: &Option<String>, mult: Option<f64>| -> String {
        match (mult, rate) {
            (Some(m), _) => fmt::mult(Some(m)),
            (None, Some(r)) => {
                let x = crate::units::dec_to_f64(r, v.decimals).unwrap_or(f64::NAN);
                format!("{} {}/{}", crate::units::fmt_sig(x, 4), v.symbol, fmt::native_sym(cfg, v.chain_id as i64))
            }
            _ => "–".into(),
        }
    };
    let content = html! {
        h1 { "Market position" }
        p.muted {
            "Wallets (Railway, Terminal Wallet) first drop every offer outside −" (cfg.waku.trusted_fee_lower * 100.0) " % / +"
            (cfg.waku.trusted_fee_upper * 100.0) " % of the trusted fee signers' average (the band), then take the cheapest remaining offer "
            "(the wallet pick; higher reliability breaks ties). Undercutting below the band makes a broadcaster invisible. "
            "The random pool (≤ cheapest × " (pool_factor) ") is the SDK's alternative selection; no wallet we know of uses it. "
            "Rates are multiples of the market price, using our own rate ÷ (1 + fee_bonus) as the reference. "
            "Only offers with a valid signature, ≥ 40 s until expiry and available wallets count."
        }
        p {
            "Waku: "
            @if !status.0 { (pill(Status::Unknown, "Disabled")) }
            @else if let Some(e) = &status.5 { (pill(Status::Critical, "Error")) " " span.muted { (e) } }
            @else { (pill(if status.1 > 0 { Status::Good } else { Status::Warning }, &format!("{} peers", status.1))) }
            " · " (status.2) " messages, " (status.3) " invalid"
            @if let Some(t) = status.4 { " · last " (fmt::when(t, now)) }
        }
        @for c in &cfg.chains {
            @let cv: Vec<_> = views.iter().filter(|v| v.chain_id == c.id).collect();
            section.card id=(format!("chain-{}", c.id)) {
                header { h2 { (c.name) } span.muted { @if let Some(b) = c.fee_bonus { "fee_bonus " (format!("{:.1}", b * 100.0)) " %" } } }
                div.tablewrap { table {
                    thead { tr {
                        th { "Token" } th.num { "Our rate" } th { "Wallets" } th.num { "Rank" } th.num { "Pick" } th.num { "Band" }
                        th.num { "Picked, 24 h" } th.num { "Change to be pick" } th { "Random pool" }
                    } }
                    tbody {
                        @for v in &cv {
                            tr {
                                td { (v.symbol) }
                                td.num {
                                    (rate_text(v, &v.our_rate, v.our_multiplier))
                                    @if v.our_source == Some("log") { " " span.muted title="Not seen on Waku; rate from our own logs" { "(log)" } }
                                }
                                td { (wallet_status(v)) }
                                td.num { @if let Some(r) = v.our_rank { (r) " / " (if v.our_eligibility == Some(Eligibility::Eligible) { v.n_eligible + usize::from(v.our_source == Some("log")) } else { v.n_broadcasters + usize::from(v.our_source == Some("log")) }) } @else { "–" } }
                                td.num { (rate_text(v, &v.cheapest, v.pick_multiplier)) }
                                td.num {
                                    @match (&v.band, v.band_multiplier) {
                                        (_, Some((lo, hi))) => { (fmt::mult(Some(lo))) "–" (fmt::mult(Some(hi))) }
                                        (Some((lo, hi)), None) => { (rate_text(v, &Some(lo.clone()), None)) "–" (rate_text(v, &Some(hi.clone()), None)) }
                                        _ => span.muted { "–" },
                                    }
                                }
                                td.num { @if let Some((s, n)) = shares.get(&(v.chain_id as i64, v.token.clone())) { span title=(format!("{n} samples")) { (format!("{:.0} %", s * 100.0)) } } @else { "–" } }
                                td.num {
                                    @match v.change_for_pick {
                                        Some(x) if x == 0.0 && v.picked == Some(true) => span.muted { "pick" },
                                        Some(x) if x.abs() < 0.0005 => span.muted title="Tie with the pick: reliability decides" { "≈ 0 % (tie)" },
                                        Some(x) => (fmt::pct(Some(x))),
                                        None => "–",
                                    }
                                }
                                td {
                                    @match v.in_pool {
                                        Some(true) => { "in (" (v.pool_size) ")" }
                                        Some(false) => span.muted { "out (" (v.pool_size) ")" },
                                        None => "–",
                                    }
                                }
                            }
                        }
                    }
                } }
                @for v in &cv {
                    @if !v.entries.is_empty() {
                        details.offers {
                            summary { (v.symbol) ": " (v.n_broadcasters) " broadcasters" }
                            div.tablewrap { table {
                                thead { tr { th.num { "Rank" } th { "Broadcaster" } th.num { "Rate" } th.num { "vs pick" } th { "Wallets" } th { "Pool" } th.num { "Reliability" } th { "Version" } th.num { "Expires in" } } }
                                tbody {
                                    @for e in &v.entries {
                                        tr class=[e.is_us.then_some("us")] {
                                            td.num { (e.rank) }
                                            td.mono title=(e.address) {
                                                @if e.is_us { strong { "us" } " " }
                                                (fmt::short(&e.address))
                                                @if let Some(i) = &e.identifier { span.muted { " " (i) } }
                                                @if e.trusted { " " span.muted title="Trusted fee signer: sets the band" { "(trusted)" } }
                                            }
                                            td.num { (rate_text(v, &Some(e.rate.clone()), e.multiplier)) }
                                            td.num { (format!("{:.3}×", e.ratio_to_cheapest)) }
                                            td {
                                                @if e.is_pick { (pill(Status::Good, "Pick")) }
                                                @else if e.eligibility == Eligibility::Eligible { "eligible" }
                                                @else { span.muted { (e.eligibility.label()) } }
                                            }
                                            td { @if e.in_pool { "in" } @else { span.muted { "out" } } }
                                            td.num { (e.reliability.map(|r| format!("{r:.2}")).unwrap_or("–".into())) }
                                            td { (e.version.clone().unwrap_or_default()) }
                                            td.num { (e.expires_in_s) " s" }
                                        }
                                    }
                                }
                            } }
                        }
                    }
                }
            }
        }
    };
    Ok(layout(app, "Market", "/market", "/market", &["market", "fees"], partial, content))
}

// ---------------------------------------------------------------------------
// Events

fn events_table(app: &App, rows: &[queries::EventRow], now: i64) -> Markup {
    html! {
        div.tablewrap { table.events {
            thead { tr { th { "Time" } th { "Level" } th { "Chain" } th { "Kind" } th { "Message" } th { "Fields" } } }
            tbody {
                @for e in rows {
                    tr class=(format!("lvl-{}", e.level.to_lowercase())) {
                        td title=(format!("{} UTC", fmt::ts(e.ts))) { (fmt::ago(e.ts, now)) }
                        td { (e.level) }
                        td { (e.chain.map(|c| app.cfg.chain_name(Some(c))).unwrap_or_default()) }
                        td.mono { (e.kind) @if let Some(r) = e.request_id { " " a href=(format!("/requests/{r}")) { "#" (r) } } }
                        td { (e.message) }
                        td.mono.detail { (e.fields.as_deref().map(|f| f.chars().take(400).collect::<String>()).unwrap_or_default()) }
                    }
                }
            }
        } }
    }
}

pub async fn events(app: &App, q: &HashMap<String, String>, partial: bool) -> Result<Markup> {
    let now = now_us();
    let kind = param(q, "kind").map(str::to_string);
    let chain = chain_param(q);
    let k2 = kind.clone();
    let (rows, counts) = app
        .db
        .read(move |c| {
            Ok((queries::events(c, k2.as_deref(), chain, None, 200)?, queries::kind_counts(c, now - DAY_US)?))
        })
        .await?;
    let url = format!(
        "/events{}",
        qs(&[("kind", kind.clone().unwrap_or_default()), ("chain", chain.map(|c| c.to_string()).unwrap_or_default())])
    );
    let content = html! {
        h1 { "Events" }
        p.muted { "Parsed broadcaster log lines (redacted). High-volume kinds are kept for the configured raw retention; their hourly counts are kept forever. "
            code { "other" } " and " code { "unparsed" } " are lines the parser doesn't recognise – a rising count means the log format drifted." }
        form.filters method="get" {
            label { "Kind "
                select name="kind" {
                    option value="" selected[kind.is_none()] { "All" }
                    @for (k, n) in &counts { option value=(k) selected[kind.as_deref() == Some(k.as_str())] { (k) " (" (n) " in 24 h)" } }
                }
            }
            label { "Chain " select name="chain" { (chain_options(app, chain)) } }
            button type="submit" { "Apply" }
        }
        (events_table(app, &rows, now))
    };
    Ok(layout(app, "Events", "/events", &url, &["health"], partial, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_ingest_state_and_hint() {
        use crate::config::SourceKind::*;
        let s = |k, age, up| log_status(&k, age, up);
        assert_eq!(s(None, Option::None, 9999).0, Status::Unknown, "no source is not an error");
        assert_eq!(s(Journald, Some(30_000_000), 9999).0, Status::Good);
        assert!(s(Journald, Option::None, 30).2.is_none(), "give it two minutes before complaining");
        let (st, label, hint) = s(Journald, Option::None, 300);
        assert_eq!((st, label), (Status::Warning, "No lines received"));
        assert!(hint.unwrap().contains("journald"));
        let (st, label, hint) = s(Command, Some(10 * 60 * 1_000_000), 9999);
        assert_eq!((st, label), (Status::Warning, "Quiet"));
        assert!(hint.unwrap().contains("source.command"));
    }

    #[test]
    fn fee_publish_freshness() {
        let h = crate::config::Health::default();
        assert!(publish_status(Some(60), &h).0 == Status::Good);
        assert!(publish_status(Some(300), &h).0 == Status::Warning);
        assert!(publish_status(Some(601), &h).0 == Status::Critical, "stale > 10 min is red");
        assert!(publish_status(None, &h).0 == Status::Unknown);
    }

    #[test]
    fn runway_prefers_actual_costs() {
        let mut h = queries::ChainHealth { balance: Some(0.05), gas_price_wei: Some(1e9), ..Default::default() };
        let (n, basis) = runway(&h).unwrap();
        assert!((n - 0.05 / (1e9 * 1.3e6 / 1e18)).abs() < 1e-6);
        assert!(basis.contains("gas price"));
        h.avg_cost = Some((0.000069, 1));
        let (n, basis) = runway(&h).unwrap();
        assert_eq!(n.round(), 725.0);
        assert!(basis.contains("1 relay"));
    }
}
