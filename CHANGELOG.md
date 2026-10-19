# Changelog

All notable changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow [Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

### Added

- First public release candidate of the dashboard (`rbm`): overview and health per chain, relays with actual cost and profit, rejected and failed requests grouped by reason, profit charts, events with log-drift counters, and a market page.
- Log sources: `stdin`, `command` (supervised `docker logs -f`, restarts with overlap) and `journald` (cursor resume).
- Waku light-client market view with signature verification. It models how wallets choose a broadcaster: the trusted-fee-signer band, then the cheapest offer, with the SDK's random 5 % pool for reference. See [docs/MARKET.md](docs/MARKET.md).
- `deploy/install.sh`, a hardened systemd unit, a Docker image and a Compose example.
- Config: `waku.trusted_fee_signers`, `trusted_fee_lower`, `trusted_fee_upper`.
- `rbm` refuses to start while the placeholder addresses from `config.example.toml` are still in the config.
- The Overview's log-ingest tile turns into a warning, with a hint matching the configured source, when no line has arrived two minutes after start or none for five minutes.

### Changed

- Log output has no ANSI colour codes when stdout isn't a terminal (journald, `docker logs`).
- RPC URLs are logged without credentials, path and query.
- The example config defaults to `storage.path = "rbm.sqlite"`; `deploy/install.sh` sets the system path.

### Fixed

- Database schema v2 adds the `picked` and `eligible` columns to `market_snapshots`; databases from earlier builds migrate automatically.

## 0.1.0

Internal development build, never tagged or released. Everything above ships as the first public release.
