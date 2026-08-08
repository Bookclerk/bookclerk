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

| Endpoint | Auth | Purpose |
| --- | --- | --- |
| `GET /health` | no | Liveness |
| `POST /api/auth/login` | no | Operator token → session cookie |
| `GET /api/status` (also `/status`) | yes | Status snapshot |
| `POST /api/library/scan` (also `/scan`) | yes | Queue scan (`Content-Type: application/json`) |
| `POST /api/library/acquire` (also `/acquire`) | yes | Queue acquire |
| `GET /api/library/books` | yes | Book rows for the GUI |
| `GET /api/jobs` (also `/jobs`) | yes | Job list |
| `/` static UI | no | Built React SPA when `ui/dist` is present |

Override listen with `BOOKCLERK_DAEMON_LISTEN` or `daemon.listen`. The Web UI and
API share this bind address. IPv4 and IPv6 are supported — use bracketed form for
IPv6 (e.g. `[::1]:8787`). Changing `daemon.listen` in config and reloading
(`POST /api/config/reload` or SIGHUP) **rebinds the HTTP listener without
restarting** the process.

Operator auth
defaults **on** (`[daemon.auth]`); token at `$BOOKCLERK_FILES_DIR/operator.token`
or `BOOKCLERK_OPERATOR_TOKEN`. Do not expose publicly without TLS (reverse
proxy) and a protected token. Details: [gui.md](gui.md).

Talk to a running daemon from the CLI (sends Bearer when auth is enabled):

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
# after installing bookclerkd, bookclerk-media-worker, and bookclerk-jail to
# /usr/local/bin and creating user/dirs:
sudo systemctl enable --now bookclerkd
```

Install all three from the same directory. `bookclerkd` looks for its helpers
beside itself and, by default, refuses media work or declines to load an external
plugin rather than running either unconfined.

Highlights from the sample unit:

- `Environment=BOOKCLERK_FILES_DIR=/var/lib/bookclerk`
- `ProtectSystem=strict` + `ReadWritePaths=/var/lib/bookclerk`
- Prefer `BOOKCLERK_AUTH_PASSWORD` (or `[auth].password`) — not under the files dir.
  Wrap an existing BCK1 key later with `bookclerk config master-key wrap` or
  reload `bookclerkd` after setting the password (SIGHUP / `POST /api/config/reload`).

If acquired media lives outside the files dir, set an absolute
`output.local.root` and add that path to `ReadWritePaths`.

## Docker

Dockerfile: [`packaging/docker/Dockerfile`](../packaging/docker/Dockerfile)
(repo-root convenience symlink: `Dockerfile`).

For day-to-day compile/test with a controlled OpenSSL/Node toolchain (and host-
runnable `target/` binaries), use the [Dev Container](devcontainer.md) instead
of fighting missing `libssl-dev` on the host.

```bash
docker build -f packaging/docker/Dockerfile -t bookclerkd .
docker run --rm \
  -v bookclerk-config:/config \
  -v bookclerk-data:/data \
  -e BOOKCLERK_AUTH_PASSWORD='your-strong-passphrase' \
  bookclerkd
```

| Path / env | Role |
| --- | --- |
| `/config` | `BOOKCLERK_FILES_DIR` (`config.toml`, `library.db` incl. `encrypted_secrets`) |
| `/data` | Default books root (`BOOKCLERK_OUTPUT_LOCAL_ROOT=/data/Audiobooks`) |
| `BOOKCLERK_DAEMON_LISTEN` | Default loopback inside the container |

To publish the UI / API (keep operator auth enabled; terminate TLS at a proxy):

```bash
docker run … -e BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787 \
  -e BOOKCLERK_OPERATOR_TOKEN_FILE=/secrets/operator.token \
  -p 8787:8787 bookclerkd
```

Copy `/etc/bookclerk/config.example.toml` from the image as a starting config.

Both confined tiers work under the default Docker seccomp profile, which has
allowed the `landlock_*` syscalls since Docker 20.10.14. On an older engine the
syscalls are refused, and `bookclerkd` says so at startup instead of quietly
running codecs and plugin guests unconfined. Do not reach for
`--security-opt seccomp=unconfined` to fix it: that removes a boundary rather
than restoring one. Upgrade the engine, or lower `isolation` deliberately.

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
