# Deployment

`rbm` runs next to your broadcaster and needs three things:

1. Access to the broadcaster's log lines.
2. Outbound HTTPS to a few public RPC endpoints.
3. Outbound TCP/WebSocket to Waku peers (light client: filter and store, no inbound port).

It never needs, and must never be given, the broadcaster's `config.yaml`, a mnemonic, or any key. See [SECURITY.md](../SECURITY.md).

## 1. Choose how `rbm` gets the logs

| Option | How | Pros | Cons |
|---|---|---|---|
| **journald** (recommended) | The broadcaster container uses `logging: driver: journald`. `rbm` runs `journalctl -f -o json CONTAINER_NAME=…` and resumes from its stored cursor. | Logs survive container recreation. `rbm` needs only the `systemd-journal` group, not root. `docker logs` keeps working. | Changes the broadcaster's compose file and needs a recreate. |
| **command** | `rbm` spawns `docker logs -f --since {since} <container>` and restarts it when the container is recreated. | No change to the broadcaster. | Needs Docker access, which is **root-equivalent**. Lines logged while the container is gone are lost. |
| **stdin** | `docker logs -f <container> 2>&1 \| rbm ingest` | Simplest; works anywhere. | You supervise the pipe yourself. Same Docker-access caveat. |

Every option is safe to restart and to overlap. Each line is deduplicated by a hash of its full text including the timestamp, the `command` source restarts with `--since <last ingested − 60 s>`, and the journald source resumes from its cursor.

Keep the broadcaster's `RUST_LOG=info,broadcaster_service=debug`. The per-chain `our fees` lines are `DEBUG`, and the fee-publish health check and the "our rate" fallback depend on them.

### Switching the broadcaster to the journald driver

This changes the broadcaster's compose file and recreates its container. Do it in a maintenance window you are comfortable with; `rbm` does not do it for you.

```yaml
# docker-compose.yml, service `broadcaster`
    logging:
      driver: journald
      options:
        tag: railgun-broadcaster
```

Then `docker compose up -d` and check that lines arrive:

```sh
journalctl -f CONTAINER_NAME=railgun-broadcaster
```

The journal keeps the logs across recreations. Bound its size with `SystemMaxUse=` in `/etc/systemd/journald.conf`.

## 2. systemd service (recommended)

### Build

Build off the broadcaster host if it is small; a broadcaster VM is often short on CPU and disk. A binary built on Debian 13 needs glibc ≥ 2.39 and `libssl3` at runtime (Debian 13, Ubuntu 24.04 or newer).

```sh
cargo build --release --locked        # → target/release/rbm (~15 MB)
mkdir bundle && cp target/release/rbm deploy/rbm.service deploy/install.sh config.example.toml bundle/
tar czf rbm-bundle.tar.gz bundle && scp rbm-bundle.tar.gz <host>:/tmp/
```

### Install

```sh
ssh -t <host> 'cd /tmp && tar xzf rbm-bundle.tar.gz && sudo ./bundle/install.sh command'   # or: journald
```

`install.sh` creates the `rbm` user, installs the binary, the unit and `/etc/rbm/rbm.toml` (mode 0640, readable by root and the service only, because it can hold RPC API keys and a basic-auth password), and, on a first install, seeds the database from the logs the container still has. On a **fresh host it stops after installing** (exit code 3) because the config still holds the placeholder addresses. Edit it, then run the script again:

```sh
sudoedit /etc/rbm/rbm.toml            # gas wallet, 0zk address, RPC URLs, fee_bonus per chain
sudo -u rbm rbm -c /etc/rbm/rbm.toml check-config
sudo ./bundle/install.sh command
```

Running the script again later upgrades the binary and the unit and leaves `/etc/rbm/rbm.toml` and the database alone. It never touches the broadcaster container, its compose file, `config.yaml` or `monitor.env`.

Prefer to do it by hand? The script is short and commented; the steps are: create a system user `rbm`; install the binary to `/usr/local/bin/rbm`; install the config to `/etc/rbm/rbm.toml` with `storage.path = "/var/lib/rbm/rbm.sqlite"` and the right `source.kind`; install `deploy/rbm.service` into `/etc/systemd/system/`; `systemctl daemon-reload && systemctl enable --now rbm`.

### What the unit does

It runs as the unprivileged user `rbm` with group `systemd-journal` and a strict sandbox (`ProtectSystem=strict`, no capabilities, a system-call filter, `MemoryMax=128M`, `CPUQuota=50%`). It also makes `/opt/railgun/src/config.yaml`, `/opt/railgun/monitor.env` and the broadcaster's database **inaccessible** to the process (`InaccessiblePaths=`). If your broadcaster lives elsewhere, adjust those paths in the unit.

With `source = command`, the installer adds a drop-in that gives the service the `docker` group. That group is root-equivalent; the sandbox narrows what the service can do with it, but doesn't remove the risk. Prefer `journald`.

## 3. Reaching the UI

`rbm` binds `127.0.0.1:8080` by default. Use an SSH tunnel; no firewall change is needed:

```sh
ssh -L 8080:localhost:8080 <host>     # then open http://localhost:8080
```

To serve it elsewhere, put a TLS-terminating reverse proxy in front, or bind a non-loopback address and set `web.basic_auth = "user:password"` (or `RBM_BASIC_AUTH`). `rbm` refuses a non-loopback bind without credentials unless `allow_unauthenticated_public_bind = true`. Basic auth over plain HTTP sends the password in clear text; use TLS.

## 4. Docker

```sh
docker build -t rbm:0.1.0 .
```

See [`deploy/docker-compose.example.yml`](../deploy/docker-compose.example.yml). The image contains `rbm` and `journalctl`, so it supports the `journald`, `stdin` and `none` sources. It does **not** contain the Docker CLI, so `source = command` doesn't work inside it. Run it with `network_mode: host` so the UI stays on the host's loopback, mount the journal read-only, add the host's `systemd-journal` gid with `group_add`, and set `storage.path = "/data/rbm.sqlite"` on a volume.

The systemd service is the better-exercised path. If journal access from inside a container gives you trouble, use the service.

## 5. Seeding older relays

`rbm` only sees what is in the log source from the moment it starts. To import relays from older logs you saved, replay them (idempotent):

```sh
sudo -u rbm rbm -c /etc/rbm/rbm.toml replay old-broadcaster.log
```

## 6. Check that it works

- `systemctl status rbm` is active, and `journalctl -u rbm -n 30` shows `starting rbm`, `web UI listening` and `waku: subscribed to fee topics`.
- The Overview's **Log ingest** tile shows a recent "last line", and **Waku fee market** shows peers.
- Each chain card shows a green **Last fee publish** within about a minute (the broadcaster publishes every minute).
- After the first relay, the Relays page shows cost and profit within a minute or two, once the receipt is available.

### Troubleshooting

| Symptom | Likely cause |
|---|---|
| Log ingest tile says "No lines received" (after two minutes) or "Quiet" | The source delivers nothing: wrong `source.kind`, wrong container name in `journald_match` / `command`, or the broadcaster isn't using the journald driver. The tile names what to check. "No live source" only means `source.kind = "none"`. |
| "No fee publish seen" on every chain | The broadcaster doesn't run with `broadcaster_service=debug`, so the `our fees` lines aren't logged. |
| Waku shows 0 peers or an error | Outbound TCP/WebSocket blocked. Check the firewall; add `waku.direct_peers` if DNS discovery is blocked. |
| "No trusted fee" in the Market page | Normal for a few minutes after start. If it persists, the trusted signers publish nothing for that token, which is what wallets would see too. |
| Many "unrecognised" lines on Events | The broadcaster's log format changed. Open an issue with a few redacted examples. |
| Balance or runway missing | The chain's RPC URLs are rate-limited or wrong; the Events page and `journalctl -u rbm` show the errors. |

## Resource budget

Measured with live Waku (four chains, every broadcaster's fee topic) and the web UI idle: about 25 MB RSS and roughly 11 s of CPU in the first 15 minutes, almost all of it the startup burst. `MALLOC_ARENA_MAX=2` saves about 1 MB more.

Disk use is dominated by:

- `events`: noisy kinds are pruned after `storage.raw_events_days` (default 14).
- `fee_samples`: every live broadcaster's rate every `waku.fee_sample_secs`, pruned after `storage.fee_history_days` (default 7); about 3 MB per day with ~130 broadcasters.
- `market_snapshots`: one row per chain and token every `waku.sample_secs`; about 0.25 MB per day.

Expect well under 200 MB per year with the defaults.
