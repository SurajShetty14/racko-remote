#!/usr/bin/env bash
# Deploy broker + racko-remote from the Actions runner workspace to the
# canonical paths used by systemd and nginx. Safe to re-run.
#
# Layout:
#   /opt/racko/broker       — broker binary
#   /opt/racko/broker.prev  — previous binary (rollback)
#   /opt/racko/broker.env   — env file (created once on the box, not deployed)
#   /var/www/racko-remote/  — frontend static files
set -euo pipefail

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BROKER_SRC="${ROOT}/target/release/broker"
INSTALL_DIR="/opt/racko"
BROKER_BIN="${INSTALL_DIR}/broker"
BROKER_PREV="${INSTALL_DIR}/broker.prev"
WEB_ROOT="/var/www/racko-remote"
DIST="${ROOT}/web-client/racko-remote/dist"
HEALTH_URL="http://127.0.0.1:9091/healthz"

# Full paths only — must match /etc/sudoers.d/racko-deploy exactly.
SYSTEMCTL=/usr/bin/systemctl
NGINX=/usr/sbin/nginx

rollback_broker() {
  if [[ -f "${BROKER_PREV}" ]]; then
    log "health check failed; restoring ${BROKER_PREV}"
    cp "${BROKER_PREV}" "${BROKER_BIN}"
    chmod 0755 "${BROKER_BIN}"
    sudo "${SYSTEMCTL}" restart racko-broker || log "rollback restart failed"
  else
    log "health check failed and no ${BROKER_PREV} exists yet"
  fi
}

log "deploy starting from ${ROOT}"

[[ -f "${BROKER_SRC}" ]] || {
  log "missing ${BROKER_SRC} (build release broker first)"
  exit 1
}
[[ -f "${DIST}/index.html" ]] || {
  log "missing ${DIST}/index.html (build racko-remote first)"
  exit 1
}

mkdir -p "${INSTALL_DIR}" "${WEB_ROOT}"

if [[ -f "${BROKER_BIN}" ]]; then
  cp "${BROKER_BIN}" "${BROKER_PREV}"
  log "backed up ${BROKER_BIN} -> ${BROKER_PREV}"
fi

cp "${BROKER_SRC}" "${BROKER_BIN}"
chmod 0755 "${BROKER_BIN}"
log "installed ${BROKER_BIN}"

rsync -a --delete "${DIST}/" "${WEB_ROOT}/"
log "synced frontend to ${WEB_ROOT}"

sudo "${SYSTEMCTL}" restart racko-broker
log "restarted racko-broker"

sudo "${NGINX}" -t
sudo "${SYSTEMCTL}" reload nginx
log "reloaded nginx"

healthy=0
for _ in 1 2 3 4 5 6; do
  if curl -fsS "${HEALTH_URL}" >/dev/null; then
    healthy=1
    break
  fi
  sleep 2
done

if [[ "${healthy}" -ne 1 ]]; then
  log "health check failed: ${HEALTH_URL}"
  rollback_broker
  exit 1
fi

log "health check ok: ${HEALTH_URL}"
log "deploy complete"
