# Racko CI/CD

Pushes and pull requests to `main` run [`.github/workflows/ci.yml`](.github/workflows/ci.yml) on GitHub-hosted Ubuntu.
A successful **push** to `main` then runs [`.github/workflows/deploy.yml`](.github/workflows/deploy.yml) on the self-hosted runner labeled `racko-deploy`.
That job checks out the commit, builds the broker and frontend on the box, and [`scripts/deploy.sh`](../scripts/deploy.sh) installs them into the canonical layout below.

The IronRDP workflow is [`.github/workflows/ironrdp-ci.yml`](.github/workflows/ironrdp-ci.yml) (`IronRDP CI`) and still runs on `master`.

No production URLs or tokens are stored in the repo.
Vite reads them from GitHub Actions secrets at deploy time.

## Canonical layout

| Path | Purpose |
| --- | --- |
| `/opt/racko/broker` | Broker binary (deployed each release) |
| `/opt/racko/broker.prev` | Previous binary for health-check rollback |
| `/opt/racko/broker.env` | Broker environment (created once on the box, **not** in git, `chmod 600`) |
| `/var/www/racko-remote/` | Frontend static files (nginx document root) |

Broker listen addresses (set in `broker.env`):

- Relay (browser WebSocket): `BIND_ADDR=0.0.0.0:7171`
- Management / health / metrics / Mode B signaling: `MANAGEMENT_ADDR=127.0.0.1:9091`

nginx serves `/` from `/var/www/racko-remote` and proxies `/ws`, `/api/`, and `/modeb/` to the broker.
`/modeb/` carries the Mode B signaling WebSocket, so its location needs the upgrade headers:

```nginx
location /modeb/ {
    proxy_pass http://127.0.0.1:9091;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 1h;
}
```

The deploy builds the broker with `--features modeb-encode`, so the box needs the GStreamer development packages that [`scripts/build-modeb-encode.sh`](../scripts/build-modeb-encode.sh) checks for.
If GStreamer or its WebRTC plugins are missing at runtime, the broker logs an error and serves Mode C only.

## One-time bootstrap

Run as root (or with sudo) on the Ubuntu box.
Replace `gisuladmin` if the deploy user differs.

### Directories and ownership

```bash
sudo mkdir -p /opt/racko /var/www/racko-remote
sudo chown -R gisuladmin:gisuladmin /opt/racko /var/www/racko-remote
```

The runner user must write those paths without sudo.
`scripts/deploy.sh` only uses sudo for `systemctl` and `nginx`.

### `/opt/racko/broker.env`

Create once on the box (never commit this file):

```bash
sudo tee /opt/racko/broker.env >/dev/null <<'EOF'
BIND_ADDR=0.0.0.0:7171
MANAGEMENT_ADDR=127.0.0.1:9091
RDP_TARGET=203.0.113.10:3389
RDP_USERNAME=Administrator
RDP_PASSWORD=change-me
RDP_TLS_VERIFY=insecure
IDLE_TIMEOUT_SECS=900
EOF
sudo chown gisuladmin:gisuladmin /opt/racko/broker.env
sudo chmod 600 /opt/racko/broker.env
```

Edit `RDP_TARGET` and credentials for the lab VM.
`MANAGEMENT_ADDR` must stay on `127.0.0.1:9091` so deploy health checks can reach `/healthz`.
Optional: `RDP_ALLOWED_TARGETS` (comma-separated `host:port`) allows more VMs for both modes, and `MODEB_MAX_SESSIONS` (default 5) caps concurrent Mode B sessions below the GPU's NVENC limit.
Mode B signs in to every allowed VM with `RDP_USERNAME` / `RDP_PASSWORD`.

### systemd unit

```bash
sudo cp deploy/racko-broker.service /etc/systemd/system/racko-broker.service
sudo systemctl daemon-reload
sudo systemctl enable --now racko-broker
```

The unit template is [`deploy/racko-broker.service`](../deploy/racko-broker.service):
`ExecStart=/opt/racko/broker`, `EnvironmentFile=/opt/racko/broker.env`, `WorkingDirectory=/opt/racko`, `User=gisuladmin`, `LimitNOFILE=1048576`.

### sudoers

Allow only the three deploy commands, with those exact paths:

```bash
sudo visudo -f /etc/sudoers.d/racko-deploy
```

```
gisuladmin ALL=(root) NOPASSWD: /usr/bin/systemctl restart racko-broker, /usr/sbin/nginx -t, /usr/bin/systemctl reload nginx
```

A password prompt fails the deploy job.

## Self-hosted runner

Install as `gisuladmin` on the Ubuntu box:

1. Open the repo **Settings → Actions → Runners → New self-hosted runner** and follow the Linux x64 steps GitHub shows.
2. When `config.sh` asks for labels, set `self-hosted,racko-deploy` (`self-hosted` is added automatically; add `racko-deploy`).
3. Install and start it as a service so it survives reboot: `sudo ./svc.sh install gisuladmin` then `sudo ./svc.sh start`.

The deploy job is `runs-on: [self-hosted, racko-deploy]`.
An unlabeled runner will not pick it up.

## GitHub secrets

In the repo, open **Settings → Environments → New environment**, name it `production`, and add:

| Secret | Example | Used for |
| --- | --- | --- |
| `VITE_GATEWAY_URL` | `wss://<domain>/ws` | WebSocket the browser opens |
| `VITE_BROKER_API_BASE` | `https://<domain>/api` | Dashboard session API |
| `VITE_MODEB_SIGNALING_URL` (optional) | `wss://<domain>/modeb/webrtc` | Mode B signaling; derived from `VITE_BROKER_API_BASE` when unset |

The deploy job sets `environment: production` and passes these secrets into the `racko-remote` Vite build, then refuses to continue if either required one is empty.
Do not commit them.

## What a deploy does

[`scripts/deploy.sh`](../scripts/deploy.sh) runs from the runner workspace after the release builds finish:

1. Backs up `/opt/racko/broker` to `/opt/racko/broker.prev` when a binary is already installed.
2. Copies `target/release/broker` to `/opt/racko/broker`.
3. `rsync --delete`s `web-client/racko-remote/dist/` onto `/var/www/racko-remote/`.
4. Runs `systemctl restart racko-broker`.
5. Runs `nginx -t` and, only if that succeeds, `systemctl reload nginx`.
6. `curl -fsS http://127.0.0.1:9091/healthz`.
   On failure it restores `broker.prev`, restarts `racko-broker`, and exits non-zero.

Manual rollback:

```bash
cp /opt/racko/broker.prev /opt/racko/broker
sudo systemctl restart racko-broker
```
