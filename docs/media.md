# Media worker pool

Every decode, encode, and packaging step runs in a short-lived
`bookclerk-media-worker` process that confines itself to the paths its job
declared. This page covers why, what the boundary actually is, how to configure
it, and what to check when media work starts failing.

## Why codecs get their own process

Acquire feeds attacker-influenced audio through three C libraries: LAME
(`mp3lame-sys`), FDK-AAC (`fdk-aac-sys`), and Symphonia's native
dependencies. Before the pool, those parsers ran on tokio's blocking thread
pool — inside the process that holds the master data encryption key and an open
handle to `library.db`. A parser bug there is a key disclosure, not a failed
book.

Three properties follow from moving that work out:

- **The codecs cannot reach key material.** The jail allows the job's declared
  inputs and one output directory. `master.key`, `library.db`, and the rest of
  `$HOME` are not in the allowlist.
- **A crash costs one book.** The worker is a separate process, so a segfault
  in a decoder surfaces as a failed job. The bug fixed in `mp3.rs` (LAME
  writing past its output buffer) would previously have taken the daemon down
  with it.
- **Concurrency is a number you chose.** `media.workers` bounds how many codec
  jobs run at once, instead of however many threads tokio's blocking pool
  happened to grow under load.

## The boundary

One job is one process. Workers are deliberately not reused: filesystem
confinement is irreversible and process-wide, so a long-lived worker would need
a jail wide enough for every job it might later be handed. Media operations run
for seconds to minutes, which makes a few milliseconds of process spawn
irrelevant next to a tight per-job allowlist.

The host writes a JSON `MediaJob` to the worker's stdin and closes it. The
worker builds its policy from the job, confines itself, runs the codec, and
writes a JSON reply to stdout. Nothing configures the jail separately from the
request, so a job can never be granted more than it declared.

| Platform | Backend | Filesystem | Syscalls | Network |
| --- | --- | --- | --- | --- |
| Linux | Landlock + seccomp-bpf | allowlist, ABI-probed | deny list | no IP sockets |
| macOS | Seatbelt (`sandbox_init`) | deny-default SBPL profile | — | denied in profile |
| Windows | AppContainer | spawn-time only | — | — |

macOS has no seccomp equivalent — Seatbelt gates operation classes rather than
syscall numbers — so the syscall layer reports as not applicable rather than as
a gap. The `(deny default)` profile already refuses `exec` and unlisted
operations, so nothing is lost.

**Windows cannot self-confine.** A process cannot drop itself into an
AppContainer after it has started; isolation is granted at `CreateProcess`.
The media pool therefore launches `bookclerk-media-worker` through
`bookclerk-jail` on Windows (same launcher plugins use), which applies an
AppContainer ACL allowlist derived from the job's read/write paths. The worker
skips self-confine and relies on that outer jail. Ship `bookclerk-jail` beside
the worker; with the default `isolation = "required"`, a missing jail refuses
media work rather than decoding unconfined.

What a job declares:

| Job | Reads | Writes |
| --- | --- | --- |
| `encode_mp3` | input file | output's parent directory |
| `remux_trimmed` | input file | output's parent directory |
| `fixup` | input file, cover art | output's parent directory |
| `package_m4b` | every part | output's parent directory |
| `align_chapters` | input file | nothing |

Write grants are directories rather than single files because the codecs stage
output through temporary files beside the destination. Packaging in particular
cannot know the final sample table until it has read every part, so it spills
the concatenated AAC payload to a scratch file first. That scratch goes in the
output directory, not `$TMPDIR`, which the jail does not grant. It is deleted
when the job ends.

The worker also starts with a scrubbed environment. It receives its job over
stdin and needs nothing from Bookclerk's configuration, so `BOOKCLERK_*`,
operator tokens, and cloud credentials are dropped at spawn; only locale,
timezone, `RUST_BACKTRACE`, and the Windows loader variables are inherited.
Otherwise a compromised codec could read the host's configuration out of its own
environment without touching the filesystem at all.

Symbolic links are granted at their **resolved target** — that is the inode the
kernel checks. Declaring a link therefore grants whatever it points at. Every
path in a job is built by the host from its own cache and output roots, so
nothing untrusted picks them, but a destination plugin that resolves output
paths from user input should canonicalize first.

## What this jail does not cover

DRM decrypt is the other path that parses attacker-influenced ISO-BMFF, and it
does not run in a media worker. Audible's Adrm and Widevine CENC decrypt run on
a blocking thread inside whichever process hosts the source plugin.

Two reasons keep it there, and only one of them is technical. Decrypt needs the
per-title content key, and the host is built so it never holds one: `fetch_title`
decrypts inside the source and hands back plaintext paths. Routing decrypt
through the pool would mean writing content keys to a worker's stdin, which
trades a parser boundary for a key-handling one. The other reason is
distribution: the decrypt code lives in a plugin that can be omitted from a build
or a package, which some regions may require of anyone shipping the core
binaries. Rolling it into `bookclerk-media` would remove that option.

The trim is split along the same line, and deliberately. Where Audible's brand
intro and outro sit is arithmetic on `chapter_info`, so
`bookclerk_media::brand_trim_range` computes the window and the plugin applies it
during the decrypt pass it was already making. Branding therefore never lands on
disk and no second copy of the file is made for it. Chapter splitting, which
operates on clear media, is a `remux_trimmed` job in the pool like any other.

The ISO-BMFF plumbing underneath is shared, and sharing it did not move that
line. `bookclerk-mp4` owns the parser, the sample table, and the progressive
remuxer for both callers; what differs is the `SampleTransform` each one passes
in. `bookclerk-media` passes `CopySamples`, which hands payloads through
untouched. The Audible plugin passes a decryptor that holds the content key and
the CENC metadata, and the remuxer never sees either. No cipher, key, or license
code links into the shared crate, so omitting the plugin from a build still omits
every part of decrypt — see [shipping without a
store](plugins.md#shipping-without-a-store), which is now a build both hosts
support and CI checks.

Two muxers also cost more than maintenance. Both used to clone the whole sample
table to trim it, once in the plugin and once in the host; the shared remuxer
selects by index instead, so a trim no longer allocates a second table per pass.

Both also copied samples a syscall at a time. An AAC frame is a few hundred
bytes, so a retail audiobook is a couple of million of them, and every one cost a
`seek`, a `read`, and a `write` on an unbuffered handle. `SampleReader` buffers
the read side and tracks position, which turns the usual case — samples in track
order — into a buffer hit instead of a seek, and the writers are buffered to
match. Measured with `cargo run --release -p bookclerk-mp4 --example bench_remux`,
a copy remux of a 3-hour book went from roughly 700 ms to 400 ms. The same
buffering covers the packaging muxer and the plugin's fragmented-DASH path, which
assemble their samples differently but copy them the same way.

Confining the plugin process is what covers the decrypt parser, and it covers
everything else a storefront does at the same time. Because first-party sources
ship as guest binaries as well as in-process adapters, a single jail reaches
Audible, Libro.fm, Chirp, and GraphicAudio together; see [the guest
jail](plugins.md#the-guest-jail). Default builds still register those adapters
in-process, so decrypt runs inside `bookclerkd` today, and that jail becomes
load-bearing when external guests are the packaged default.

## Configuration

```toml
[media]
workers = 0             # 0 derives one per core, capped at 8
isolation = "required"  # required | best-effort | off
# worker_bin = "/usr/local/bin/bookclerk-media-worker"
```

Environment overrides win over TOML, as everywhere else:
`BOOKCLERK_MEDIA_WORKERS`, `BOOKCLERK_MEDIA_ISOLATION`,
`BOOKCLERK_MEDIA_WORKER`.

`isolation` picks what happens when a confinement layer does not engage:

- **`required`** (default) — refuse the job. A host that cannot sandbox should
  not decode untrusted audio in the same process as the master key.
- **`best-effort`** — apply what the platform supports and log the rest. For
  kernels without Landlock, accepting that codecs run unconfined there.
- **`off`** — run codec work in-process on a blocking thread, no jail, no child
  process. Development only.

`best-effort` covers layers the *platform* cannot enforce. It does not cover a
worker binary that was never installed: that is a packaging error, and both
modes refuse jobs rather than silently fall back to in-process execution. Set
`isolation = "off"` if unconfined codecs are genuinely what you want.

The pool decides at startup, not at first acquire, so a host that cannot honour
its configured mode says so in the startup log rather than failing every book
later.

**`[media]` reloads without a restart.** A reload that changes the table builds a
new pool, points subsequent jobs at it, and lets the old one drain. Nothing is
interrupted and nothing waits: callers take a handle to the pool and hold it for
the length of their job, so the retired pool stays alive until its last job
finishes and is then dropped.

Jobs already running keep the isolation they started with, which is the only
possible answer — a worker applies its confinement inside the child process at
spawn, and that cannot be changed afterwards. So lowering `isolation` never
loosens a job that is already in a jail, and raising it never retroactively
confines one.

While the old pool drains, total codec concurrency can briefly reach its
in-flight count plus the new pool's limit. That overshoot only shrinks, because a
retired pool is unreachable and never admits another job. It is worth knowing
about if you are lowering `media.workers` to relieve memory pressure: the new
limit applies immediately to new jobs, but jobs already encoding run to
completion.

## Installing the worker

The pool looks for the worker in this order:

1. `media.worker_bin` in `config.toml`
2. `BOOKCLERK_MEDIA_WORKER`
3. `bookclerk-media-worker` beside the running executable

Each candidate is checked for existence, so a stale configured path fails
loudly instead of degrading to an unconfined encode. Build and ship it with the
hosts, alongside the plugin jail's launcher, which is found the same way:

```bash
cargo build --release -p bookclerk-cli -p bookclerkd \
  -p bookclerk-media-worker -p bookclerk-jail
```

The Docker images copy it into `/usr/local/bin` alongside `bookclerk` and
`bookclerkd`.

## Troubleshooting

**`refusing to run <job> unconfined: media isolation is required but
unavailable`** — either the worker binary was not found (a configured path does
not exist, or nothing sits beside the host executable) or the platform has no
self-confinement primitive. The startup log line says which, naming the
directory searched or the backend that came up short. Install the worker beside
the host binary, point `media.worker_bin` at it, or drop to `best-effort` /
`off` if you accept unconfined codecs.

**`media worker (<job>) failed: worker exited with <status> before replying`** —
the worker died mid-job. Its stderr is inherited by the host, so the
confinement summary and any codec output land in the daemon log just above.

**`media worker (<job>) failed: worker reported success but exited with
<status>`** — the worker wrote a success reply and then died anyway, which it
has no legitimate path to do. Usually the OOM killer or an external signal
arriving in the moment between the reply and the exit. The job fails rather
than being recorded, because a process that was killed cannot vouch for the
file it just wrote. Check the host's memory pressure and lower
`media.workers` if encoders are being killed under load.

**Jobs fail with a missing input that is clearly on disk** — the path was not
in `read_paths()` for that job kind, so the jail denied it. Adding a new input
to a job means adding it there too; the worker validates declared inputs before
running the codec so this reports the path rather than a bare `No such file`
from inside a decoder.

**Everything is slow with plenty of idle cores** — `media.workers` is the cap.
Note that acquires themselves are still serialized daemon-wide by the job
lock; the pool bounds codec concurrency within an acquire, not across them.

## Related

- [Architecture](architecture.md) — where the pool sits in the acquire pipeline
- [The guest jail](plugins.md#the-guest-jail) — the other confined tier, for
  plugin processes: longer-lived, wider grant, applied by a launcher
- [Configuration](configuration.md) — the full `config.toml` surface
- [Operations](operations.md) — running `bookclerkd`
