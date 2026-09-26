# Racko CI/CD

Pushes and pull requests to `main` run `.github/workflows/ci.yml` on GitHub-hosted Ubuntu. A successful **push** to `main` then runs `.github/workflows/deploy.yml` on the self-hosted runner labeled `racko-deploy`. The runner checks out that commit, builds the broker and the frontend on the box, and `scripts/deploy.sh` installs them.

The IronRDP workflow that used to live in `ci.yml` is now `.github/workflows/ironrdp-ci.yml` (`IronRDP CI`). It still runs on `master`.

No production URLs or tokens are stored in the repo. Vite reads them from the GitHub Actions secrets at deploy time.

## Self-hosted runner

On the Ubuntu box, as the user that should own the deploy (below, `racko`):

1. Install the GitHub Actions runner from the repo's **Settings → Actions → Runners → New self-hosted runner**, using the Linux x64 instructions GitHub shows there.
2. When `config.sh` asks for labels, set `self-hosted,racko-deploy`. `self-hosted` is added automatically; add `racko-deploy`.
3. Install and start it as a service (`sudo ./svc.sh install racko` then `sudo ./svc.sh start`) so it survives reboot.

The deploy job is `runs-on: [self-hosted, racko-deploy]`, so an unlabeled runner will not pick it up.

The runner user must be able to write these paths without sudo:

- `/opt/racko` — installed broker binary (`/opt/racko/broker`) and the rollback copy (`/opt/racko/broker.prev`)
- `/var/www/racko-remote` — nginx document root for the frontend

```bash
sudo mkdir -p /opt/racko /var/www/racko-remote
sudo chown -R racko:racko /opt/racko /var/www/racko-remote
```

`racko-broker.service` must start that binary. Example:

```ini
[Service]
ExecStart=/opt/racko/broker
EnvironmentFile=/opt/racko/broker.env
Restart=on-failure
```

`/opt/racko/broker.env` is created on the box (it is not in git). Point `MANAGEMENT_ADDR` at `127.0.0.1:9091` so the deploy health check can reach `/healthz`. The public site is nginx: `/` serves `/var/www/racko-remote`, and `/ws` and `/api/` proxy to the broker.

## sudoers

The runner may restart the broker and reload nginx, and nothing else. As root:

```bash
sudo visudo -f /etc/sudoers.d/racko-deploy
```

```
racko ALL=(root) NOPASSWD: /usr/bin/systemctl restart racko-broker, /usr/sbin/nginx -t, /usr/bin/systemctl reload nginx
```

`scripts/deploy.sh` calls those three commands with those exact paths. A password prompt fails the deploy.

## GitHub secrets

In the repo, open **Settings → Environments → New environment**, name it `production`, and add:

| Secret | Example | Used for |
| --- | --- | --- |
| `VITE_GATEWAY_URL` | `wss://<domain>/ws` | WebSocket the browser opens |
| `VITE_BROKER_API_BASE` | `https://<domain>/api` | Dashboard session API |

The same two names work as repository secrets if you prefer not to use an environment. The deploy job declares `environment: production`, so environment secrets take precedence. The deploy step refuses to build the frontend when either value is empty. Do not commit them.

## What a deploy does

`scripts/deploy.sh` is safe to run again:

1. Copies the current `/opt/racko/broker` to `/opt/racko/broker.prev` when one is already installed.
2. Installs `target/release/broker` and runs `systemctl restart racko-broker`.
3. `rsync --delete`s `web-client/racko-remote/dist/` onto `/var/www/racko-remote`.
4. Runs `nginx -t` and, only if that succeeds, `systemctl reload nginx`.
5. `curl -f http://127.0.0.1:9091/healthz`. On failure it copies `broker.prev` back, restarts `racko-broker`, and exits non-zero. To revert by hand: `install -m 0755 /opt/racko/broker.prev /opt/racko/broker && sudo systemctl restart racko-broker`.
