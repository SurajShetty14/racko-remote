#!/usr/bin/env bash
# Install the broker binary and the Racko Remote static files on this machine.
# Safe to re-run. The previous broker binary is kept as broker.prev.
# If the health check fails, that previous binary is restored and the service
# is restarted, then this script exits non-zero.
set -euo pipefail

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BROKER_SRC="${ROOT}/target/release/broker"
INSTALL_DIR="${RACKO_INSTALL_DIR:-/opt/racko}"
BROKER_BIN="${INSTALL_DIR}/broker"
BROKER_PREV="${INSTALL_DIR}/broker.prev"
WEB_ROOT="${RACKO_WEB_ROOT:-/var/www/racko-remote}"
DIST="${ROOT}/web-client/racko-remote/dist"
HEALTH_URL="${RACKO_HEALTH_URL:-http://127.0.0.1:9091/healthz}"

# These full paths are the only commands allowed in the runner sudoers file.
SYSTEMCTL=/usr/bin/systemctl
NGINX=/usr/sbin/nginx

rollback_broker() {
  if [[ -f "${BROKER_PREV}" ]]; then
    log "health check failed; restoring ${BROKER_PREV}"
    install -m 0755 "${BROKER_PREV}" "${BROKER_BIN}"
    sudo "${SYSTEMCTL}" restart racko-broker || log "rollback restart failed"
  else
    log "health check failed and no ${BROKER_PREV} exists yet"
  fi
}

log "deploy starting from ${ROOT}"

[[ -f "${BROKER_SRC}" ]] || {
  log "missing ${BROKER_SRC}"
  exit 1
}
[[ -f "${DIST}/index.html" ]] || {
  log "missing ${DIST}/index.html"
  exit 1
}

mkdir -p "${INSTALL_DIR}" "${WEB_ROOT}"

if [[ -f "${BROKER_BIN}" ]]; then
  install -m 0755 "${BROKER_BIN}" "${BROKER_PREV}"
  log "kept previous broker at ${BROKER_PREV}"
fi

install -m 0755 "${BROKER_SRC}" "${BROKER_BIN}"
log "installed ${BROKER_BIN}"
sudo "${SYSTEMCTL}" restart racko-broker
log "restarted racko-broker"

rsync -a --delete "${DIST}/" "${WEB_ROOT}/"
log "synced frontend to ${WEB_ROOT}"

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
