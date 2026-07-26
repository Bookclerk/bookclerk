# Operations

## Daemon (`bookclerkd`)

```bash
export BOOKCLERK_FILES_DIR=/var/lib/bookclerk
bookclerkd
```

Reads `config.toml` from the files dir (or `BOOKCLERK_CONFIG`). Schedules:

- library scan every `library.scan_interval_minutes`
- auto-acquire when `library.auto_acquire = true` (keep **false** until ready)

HTTP control plane (default `127.0.0.1:8787`):

| Endpoint | Purpose |
| --- | --- |
| `GET /health` | Liveness |
| `GET /status` | Status snapshot |
| `POST /scan` | Queue scan (`Content-Type: application/json`, body `{}` ok) |
| `POST /acquire` | Queue acquire |
| `GET /jobs` | Job list |

Override listen with `BOOKCLERK_DAEMON_LISTEN` or `daemon.listen`.
**Unauthenticated** — do not expose publicly without a reverse proxy / ACL.

Talk to a running daemon from the CLI:

```bash
bookclerk daemon health
bookclerk daemon status
bookclerk daemon jobs
bookclerk daemon scan [--account <id>]
bookclerk daemon acquire [--asin <id>] [--account <id>]
```

## systemd

Sample unit: [`packaging/systemd/bookclerkd.service`](../packaging/systemd/bookclerkd.service).

```bash
# after installing bookclerkd to /usr/local/bin and creating user/dirs:
sudo systemctl enable --now bookclerkd
```

Highlights from the sample unit:

- `Environment=BOOKCLERK_FILES_DIR=/var/lib/bookclerk`
- `ProtectSystem=strict` + `ReadWritePaths=/var/lib/bookclerk`
- Prefer `BOOKCLERK_AUTH_PASSWORD` or `LoadCredential=` +
  `BOOKCLERK_AUTH_PASSWORD_FILE` (not under `Accounts/`)

If acquired media lives outside the files dir, set an absolute
`output.local.root` and add that path to `ReadWritePaths`.

## Docker

Dockerfile: [`packaging/docker/Dockerfile`](../packaging/docker/Dockerfile)
(repo-root convenience symlink: `Dockerfile`).

```bash
docker build -f packaging/docker/Dockerfile -t bookclerkd .
docker run --rm \
  -v bookclerk-config:/config \
  -v bookclerk-data:/data \
  -e BOOKCLERK_AUTH_PASSWORD_FILE=/secrets/auth_password \
  -v bookclerk-secrets:/secrets \
  bookclerkd
```

| Path / env | Role |
| --- | --- |
| `/config` | `BOOKCLERK_FILES_DIR` (`config.toml`, `library.db`, `Accounts/`) |
| `/data` | Default books root (`BOOKCLERK_OUTPUT_LOCAL_ROOT=/data/Audiobooks`) |
| `BOOKCLERK_DAEMON_LISTEN` | Default loopback inside the container |

To publish the control plane:

```bash
docker run … -e BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787 -p 8787:8787 bookclerkd
```

Copy `/etc/bookclerk/config.example.toml` from the image as a starting config.

## Logging

- stderr + OS facility (journald / macOS `os_log` / Windows Event Log)
- `BOOKCLERK_LOG` → `RUST_LOG` → default `bookclerk=info,warn`
- CLI: `-v` / `-vv`
- Secrets are redacted; diagnostics uploads abort if a registered secret remains

Opt-in crash/error reporting: [diagnostics.md](diagnostics.md).

## Safe live testing

When exercising real store credentials:

1. Keep `library.auto_acquire = false`.
2. After login, `bookclerk auth set-scan <account> --scan false`.
3. Acquire **one** title (`--asin` / `--isbn` / UUID), not the whole library.
4. Prefer CLI over `POST /scan` / `/acquire` so nothing bulk-queues.
5. One-shot sync without re-enabling scan: `library scan --account <id>`.
