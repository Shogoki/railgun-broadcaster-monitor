# Security

`rbm` runs next to a machine that holds real funds and signing keys, so its design starts from one rule: **it must never be able to touch them.**

## Reporting a vulnerability

Please report security problems **privately** through GitHub: open the repository's **Security** tab and choose **Report a vulnerability**. Don't open a public issue or pull request for anything that could expose keys, operator data or the broadcaster host.

Include what you found, how to reproduce it, and the version (`rbm --version`) or commit. You'll get an answer as soon as the maintainer can give one; this is a small volunteer project, so there is no fixed response time.

Only the latest release and `main` are supported.

## What `rbm` guarantees

- **No key material.** The config has no field for a mnemonic, private key or viewing key, and `rbm` never reads the broadcaster's `config.yaml` or `monitor.env`. The provided systemd unit makes those paths inaccessible to the process (`InaccessiblePaths=`).
- **Read-only.** It doesn't write to, signal, or restart the broadcaster, and it never opens the broadcaster's database. It reads log lines, queries public RPCs, and listens on the public Waku network.
- **Redaction before storage.** Every run of 64 or more hex digits (with or without `0x`) is shortened to `0x047c…75cb` before it is persisted, except transaction hashes and purely decimal values. The acceptance test `no_key_material_in_db` replays the fixtures, dumps the whole database, and fails if any other 32-byte hex value survives.
- **Private by default.**
  - The web UI binds `127.0.0.1:8080`.
  - A non-loopback bind is refused unless `web.basic_auth` / `RBM_BASIC_AUTH` is set (or you explicitly allow it).
  - Basic-auth credentials are compared in constant time.
  - A restrictive Content-Security-Policy is sent (scripts only from the same origin, no inline scripts), and the browser makes no third-party requests.
  - There is no share, export or telemetry feature, and `rbm` never phones home.
- **Untrusted input is data.** Log lines and Waku messages are parsed defensively; unknown or malformed input is counted, not executed or trusted. Fee messages without a valid signature are never used.

## What it does not protect against

- **Anyone who can open the UI** sees your operational data: balances, profit, rejections and their timing. Treat access to the UI like access to the logs.
- **The `docker` group is root-equivalent.** With `source = command`, `rbm` needs it. The sandbox narrows what the service can do, but a bug in `rbm` would then run with that group. Prefer the `journald` source, which needs only `systemd-journal`.
- **Network metadata.** Your configured RPC providers see which receipts and balances you query (your gas wallet address and transaction hashes), and Waku peers see that your IP runs a light client. Use your own RPC node if that matters to you.
- **Secrets in `rbm.toml`.** RPC URLs may contain API keys and `web.basic_auth` a password. `deploy/install.sh` installs the file `0640 root:rbm`; keep it out of version control. `rbm` redacts URL credentials in its own logs.
- **Compromised host.** Anyone with root on the host can read everything, as always.

## Handling secrets in this repository

- Never commit a mnemonic, private key, viewing key, `config.yaml`, `monitor.env`, a notification topic or webhook URL, or a real `rbm.toml`. `.gitignore` excludes `rbm.toml` and `*.sqlite*`.
- Log fixtures must follow the rules in [tests/fixtures/README.md](tests/fixtures/README.md).
- If you ever find a secret in the history, report it privately as above; it will need rotating, not just deleting.
