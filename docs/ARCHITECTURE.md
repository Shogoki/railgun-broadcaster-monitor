# Architecture

`rbm` is one Rust binary (tokio + axum) with a SQLite database. It never writes to anything the broadcaster owns and never holds a key.

```
log source ─► parser ─► events (dedup by hash) ─► correlator ─► requests / relays
(stdin|command|journald)   │                                        │
                           └─► hourly counters, our published fees  ▼
RPC workers: receipts (cost, status, tx.to → route), balances, Chainlink native/USD
Waku light client: every broadcaster's signed fee offers ─► market views + snapshots
axum + maud + htmx + uPlot: server-rendered pages, SSE "something changed" ticks
```

## Source layout

| Path | Role |
|---|---|
| `src/main.rs` | CLI (`run`, `ingest`, `replay`, `parse`, `check-config`, `waku-probe`) and task wiring |
| `src/config.rs` | TOML config, validation, placeholder guard |
| `src/sources.rs` | `stdin` / `command` / `journald` log sources, batching, restart with overlap |
| `src/parse.rs` | ANSI stripping, `tracing` line parsing, classification into `Kind`s (versioned) |
| `src/redact.rs` | Redaction of 32-byte hex values before anything is stored |
| `src/ingest.rs` | Idempotent ingest and request/relay correlation |
| `src/db.rs` | Schema, migrations, reader/writer connections, pruning |
| `src/rpc.rs`, `src/enrich.rs` | RPC pool; receipts, balances, Chainlink prices, runway, container state |
| `src/market.rs`, `src/waku.rs` | Fee-message decoding and signature check; wallet-selection model; Waku listener and sampler |
| `src/web/` | axum routes, maud pages, SQL queries, formatting |
| `assets/` | CSS, JS, and vendored htmx / uPlot, embedded in the binary |
| `tests/` | Acceptance tests (mock JSON-RPC plus real log fixtures) in `tests/fixtures/` |
| `deploy/` | systemd unit, install script, Docker Compose example |

## Parsing and correlation

- **Format.** The broadcaster logs `tracing` fmt lines with ANSI colours. `rbm` strips the colours and handles quoted and Rust-`Debug` values, nested spans (`service{chain_id=1}:sync_live:`), and the `Ok("…")`-wrapped JSON of `our fees`.
- **Rejections.** `failed to handle transact request` carries no chain span. The broadcaster handles Waku transact messages one at a time, so the line belongs to the most recent `parsed transact calldata`. `Decrypt` and `Parse` errors happen before that line, so they stay chain-less. The `Debug` error becomes a grouping key: `Poi(ValidateList { …, source: InvalidSnarkProof })` turns into `Poi/ValidateList/InvalidSnarkProof`.
- **Relays.** Each chain has one sequential tx worker: `converting value to gas token` → `estimated gas` → `tx broadcasted`* → `submitted tx`, or a terminal failure. Lines are attributed per chain, first-in first-out, and cross-checked against fee token and amount. Stage preference stops a stale request from taking a newer request's lines.
- **State lives in SQLite**, not in memory: a request is a row with `outcome='pending'` that later lines update. Restarting `rbm` mid-request loses nothing. A request without an outcome after 10 minutes of *log* time becomes `unknown`.
- **Idempotent ingest.** Every ANSI-stripped line is hashed (FNV-1a, 64 bit, over the full line including its timestamp). `events` has a unique index on the hash and correlation runs only for new lines. That makes replays, the `--since` overlap of the `command` source, and the stored `journald` cursor all safe to repeat.
- **Log drift.** Lines that parse but aren't understood become `other`; lines that don't parse become `unparsed`. Both are counted per hour and listed on the Events page. `PARSER_VERSION` and `PARSER_TARGET` in `src/parse.rs` record which broadcaster commit the parser was written against.
- **Retention.** High-volume kinds (`our fees`, sync and price warnings) are aggregated into `hourly_counters` immediately; their raw rows are pruned after `storage.raw_events_days`. Relay-relevant events are kept.

## Numbers

- **Fee value** is the broadcaster's own conversion of the fee into the gas token (`refund` in `estimated gas`). If that wasn't logged: `fee_amount × 1e18 × (1 + fee_bonus) / our published rate`.
- **Actual cost** = `gasUsed × effectiveGasPrice` from the receipt. Receipts are final once fetched and cached forever.
- **Profit** = fee value − actual cost. **Margin** = profit ÷ actual cost.
- **USD** uses Chainlink native/USD (`latestRoundData`) nearest to the relay time. The fallback is the price implied by our own published stablecoin rates; no third-party price API is called.
- **Route.** A transaction sent to the chain's RelayAdapt contract went through the `mev` queue, anything else through `mempool`. The log doesn't name the queue, so this is inferred from the receipt.
- **Runway** = balance ÷ average actual cost of the last 20 relays. Fallbacks, in order: the last logged gas estimate, then gas price × 1.3 M gas.
- **Fee-publish freshness** is the age of the last `our fees` line per chain: green under `health.fee_publish_warn_secs`, amber up to `fee_publish_stale_secs`, red beyond.

## RPC hygiene

Per-chain URL lists, round-robin with a cooldown per endpoint after errors or HTTP 429, JSON-RPC batching (≤ 10 calls), a `User-Agent` header, and backoff for receipts that are not yet available. Only public data is requested, through four methods: `eth_getTransactionReceipt`, `eth_getBalance`, `eth_gasPrice`, and `eth_call` for the Chainlink feeds (`latestRoundData`, `decimals`).

## Storage

SQLite in WAL mode with one writer and one reader connection. The schema version is in `meta`; `src/db.rs` migrates older databases forward and refuses newer ones. Tables: `events`, `requests`, `relays`, `hourly_counters`, `chain_state`, `our_fees`, `our_fee_history`, `restarts`, `balances`, `prices`, `broadcasters`, `fee_samples`, `market_snapshots`. Expect well under 200 MB per year with the defaults; see [DEPLOYMENT.md](DEPLOYMENT.md#resource-budget).

The broadcaster's own database is never opened.

## Web

Server-rendered HTML (maud) with htmx. When the data changes the server sends a coalesced SSE tick (at most one every 2 s) and the page re-fetches its `main` element. Charts are uPlot with a small tooltip plugin. All assets are embedded with `rust-embed`; the browser makes no third-party requests, and a strict CSP is set. Colours come from a validated light/dark palette, and status is always conveyed by an icon plus a label, never colour alone.

## Design decisions

| Decision | Why |
|---|---|
| Rust, single binary | Small footprint next to a broadcaster that already runs on a modest VM; easy to ship and sandbox. |
| Parse logs instead of patching the broadcaster | The broadcaster should stay untouched and unmodified; a structured event stream upstream would be better and is on the roadmap. |
| `journald` recommended, `command` and `stdin` supported | The journal survives container recreation and needs no root-equivalent access; the other two need no change to the broadcaster. |
| Chainlink only for USD | Read on-chain through the RPCs already configured: no third-party API, key or rate limit. |
| Waku client from `railgun-rust`, behind a Cargo feature | Reuses the maintained light client; `--no-default-features` builds without it. |
| Signature check done by `rbm` | The broadcaster doesn't verify fee messages it receives, so an unsigned or forged offer must not skew the market view. The key is the Ed25519 viewing key inside the sender's 0zk address. |
| One broadcaster per process and database | Keeps correlation and the UI simple; run one `rbm` per broadcaster. |
