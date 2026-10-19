#!/usr/bin/env bash
# Install or upgrade rbm as a systemd service on the broadcaster host.
#
#   sudo ./install.sh command    # follow `docker logs` (rbm joins group docker: root-equivalent)
#   sudo ./install.sh journald   # follow the journal (broadcaster must use the journald logging driver)
#
# Run from the unpacked bundle directory (rbm, rbm.service, config.example.toml).
# On a fresh host the first run installs everything and stops (exit 3) until you have put your
# own gas wallet and 0zk address into /etc/rbm/rbm.toml; run it again afterwards.
# Never touches the broadcaster container, its compose file, config.yaml or monitor.env.
# Re-running upgrades the binary and unit; an existing /etc/rbm/rbm.toml and DB are kept.
set -euo pipefail
umask 077

MODE=${1:-}
CONTAINER=${CONTAINER:-railgun-broadcaster}
CONF=/etc/rbm/rbm.toml
DB_DIR=/var/lib/rbm
DROPIN=/etc/systemd/system/rbm.service.d/source-command.conf
cd "$(dirname "$0")"

case "$MODE" in
  command | journald) ;;
  *) echo "usage: sudo $0 command|journald" >&2; exit 2 ;;
esac
[ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
for f in rbm rbm.service config.example.toml; do [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }; done
./rbm --version

if [ "$MODE" = command ]; then
  # Only the name, never the full inspect output (it contains the container's environment).
  docker inspect -f '{{.Name}} {{.State.Status}}' "$CONTAINER"
fi

id rbm >/dev/null 2>&1 || useradd --system --home "$DB_DIR" --shell /usr/sbin/nologin rbm
install -d -o rbm -g rbm -m 0700 "$DB_DIR"
install -m 0755 rbm /usr/local/bin/rbm
install -d -m 0755 /etc/rbm
if [ ! -f "$CONF" ]; then
  sed -e "s|^kind = \"journald\"|kind = \"$MODE\"|" \
      -e "s|^path = \"rbm.sqlite\"|path = \"$DB_DIR/rbm.sqlite\"|" \
      -e 's|^# command = \[|command = [|' \
      config.example.toml > "$CONF.new"
  # RPC URLs may carry API keys and web.basic_auth a password: readable by root and the service only.
  install -m 0640 -o root -g rbm "$CONF.new" "$CONF" && rm -f "$CONF.new"
  echo "installed $CONF (source = $MODE)"
else
  echo "keeping existing $CONF"
fi
install -m 0644 rbm.service /etc/systemd/system/rbm.service

if [ "$MODE" = command ]; then
  install -d -m 0755 "$(dirname "$DROPIN")"
  printf '%s\n' '[Service]' \
    '# source = command runs `docker logs -f`; the docker group is root-equivalent.' \
    'SupplementaryGroups=docker' \
    "Environment=DOCKER_CONFIG=$DB_DIR/.docker" > "$DROPIN"
  chmod 0644 "$DROPIN"
else
  rm -f "$DROPIN"
fi
systemctl daemon-reload

# A fresh config still has the example addresses: stop here, everything else is in place.
if ! err=$(sudo -u rbm /usr/local/bin/rbm -c "$CONF" check-config 2>&1 >/dev/null); then
  echo >&2
  echo "rbm is installed but not started: $err" >&2
  echo "Edit $CONF, check it with 'sudo -u rbm rbm -c $CONF check-config', then run this script again." >&2
  exit 3
fi

# First install: seed the DB from the logs the current container still has. Lines are
# deduplicated, so the overlap with the live source is harmless.
if [ ! -f "$DB_DIR/rbm.sqlite" ]; then
  seed=$(mktemp -p "$DB_DIR" seed.XXXXXX.log)
  trap 'rm -f "$seed"' EXIT
  if [ "$MODE" = command ]; then
    docker logs "$CONTAINER" > "$seed" 2>&1
  else
    journalctl -o cat --no-pager CONTAINER_NAME="$CONTAINER" > "$seed" || true
  fi
  echo "seeding from $(wc -l < "$seed") existing log lines"
  chown rbm:rbm "$seed"
  sudo -u rbm RBM_LOG=warn /usr/local/bin/rbm -c "$CONF" replay "$seed"
  rm -f "$seed"
fi

systemctl enable rbm >/dev/null
systemctl restart rbm
sleep 8
systemctl --no-pager --lines=0 status rbm || true
journalctl -u rbm --no-pager -n 15 -o cat
echo
echo "UI: ssh -L 8080:localhost:8080 <this host>, then open http://localhost:8080"
