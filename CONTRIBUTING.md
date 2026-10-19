# Contributing

Thanks for helping. This is a small tool with a narrow job, so the most useful contributions are bug reports with log examples, support for log or broadcaster changes, and small, well-tested improvements.

## Ground rules

These keep the project safe to run next to a machine that holds funds. Changes that break them won't be merged:

1. **Read-only, no keys.** No feature may accept, read, store or transmit a mnemonic, private key or viewing key, or modify, restart or control the broadcaster.
2. **Private by default.** No telemetry, no third-party requests from the server beyond the configured RPCs and Waku, none from the browser.
3. **Small.** The target is a single binary using roughly 30 MB of RAM. Think twice before adding a dependency; prefer what's already in `Cargo.lock`.
4. **Robust to drift.** Unknown input is counted and shown, never fatal.

For anything security-sensitive, follow [SECURITY.md](SECURITY.md) instead of opening a public issue.

## Reporting bugs and log drift

Please include `rbm --version`, your `source.kind`, and what you expected. If a broadcaster update changed its log format, the **Events** page lists the unrecognised lines. Paste a few of them, **after removing anything private** (the stored events are already redacted, but check). `rbm parse FILE` prints how the parser classified each line as JSON.

## Development

```sh
cargo build
cargo test                                   # unit tests + acceptance tests (mock JSON-RPC, real log fixtures)
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --no-default-features            # without the Waku client
```

CI runs the same commands. `rustfmt.toml` sets a 120-column width.

Run the dashboard against the sample logs (put any two valid-looking addresses in `rbm.toml` first; the placeholders from the example are refused):

```sh
cp config.example.toml rbm.toml
cargo run -- replay tests/fixtures/*.log
cargo run -- run --source none               # http://127.0.0.1:8080
```

`rbm waku-probe --chain 137` talks to the real Waku network and prints the fee market; the tests never touch the network.

### Where things are

See the source layout in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The market model is explained in [docs/MARKET.md](docs/MARKET.md), which also records which wallet code it was checked against.

### Changing the parser

- Add the real log line to a file in `tests/fixtures/` (rules in [tests/fixtures/README.md](tests/fixtures/README.md)) and a test that asserts how it is classified and correlated.
- If you change what existing lines mean, bump `PARSER_VERSION` in `src/parse.rs`. If you verified the parser against a newer broadcaster commit, update `PARSER_TARGET` and the compatibility section of the README.
- New high-volume kinds belong in `NOISY_KINDS` (aggregated, then pruned) rather than as permanent rows.

### Changing the database

Bump `SCHEMA_VERSION` in `src/db.rs`, add a forward migration, and add a test like `migrates_v1_market_snapshots`. Databases from older releases must keep opening.

### Changing the UI

- Pages are server-rendered with maud; keep them working without JavaScript where practical (charts are the exception).
- Use the colour tokens in `assets/app.css` for both light and dark themes. Status is always an icon **and** a label, never colour alone.
- Vendored assets live in `assets/vendor/` with their licenses; don't load anything from a CDN.

### Tests

- Prefer real fixtures over invented lines.
- Never let a test depend on the live network, the clock's date, or the author's addresses.
- Keep `no_key_material_in_db` passing: it is the safety net for the redaction rules.

## Pull requests

- Keep them small and focused; explain *why* in the description.
- Add a line under **Unreleased** in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.
- Make sure `cargo fmt`, `cargo clippy -D warnings` and `cargo test` pass.
- By contributing you agree that your work is released under the [MIT license](LICENSE).
