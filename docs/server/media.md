# Media worker pool

This page explains the media worker: the confined helper process that
decodes, encodes and packages audio. It covers how to install and
configure it, and what to check when media work fails.

## What it does

Every decode, encode and packaging step runs in a short-lived
`bookclerk-media-worker` process, confined to the paths its job declared.
On Linux and macOS the worker confines itself; on Windows `bookclerk-jail`
confines it at spawn.

Acquire feeds audio from outside sources through three C libraries: LAME
(`mp3lame-sys`), FDK-AAC (`fdk-aac-sys`) and Symphonia's native
dependencies. Before the pool, those parsers ran on tokio's blocking
threads, inside the process that holds the master encryption key and an
open `library.db`. A parser bug there would have disclosed the key, not
just failed a book.

Moving the work out gives three properties:

- **The codecs can't reach key material.** The jail allows the job's
  declared inputs and one output folder. `master.key`, `library.db` and
  the rest of `$HOME` are not allowed.
- **A crash costs one book.** A decoder segfault becomes a failed job,
  not a dead daemon. The LAME bug fixed in `mp3.rs` (writing past its
  output buffer) used to take the daemon down with it.
- **You choose the concurrency.** `media.workers` caps how many codec jobs
  run at once, instead of however many threads tokio's blocking pool
  grows under load.

## Install the worker

Build the worker and ship it with the hosts, beside the plugin jail's
launcher (found the same way):

```sh
cargo build --release -p bookclerk-cli -p bookclerkd -p bookclerk-media-worker -p bookclerk-jail
```

The Docker image ships it in the read-only bundle under `/opt/bookclerk`,
next to `bookclerk` and `bookclerkd` (see
[Running Bookclerk → Docker](operations.md#docker)).

The pool looks for the worker in this order:

1. `media.worker_bin` in `config.toml`
2. `BOOKCLERK_MEDIA_WORKER`
3. `bookclerk-media-worker` beside the running executable

Each candidate must exist, so a stale configured path fails loudly
instead of quietly encoding unconfined.

## Configure it

```toml
[media]
workers = 0             # 0 derives one per core, capped at 8; 1 when memory.max <= 1 GiB or cpu.max is one core
isolation = "required"  # required | best-effort | off
# worker_bin = "/usr/local/bin/bookclerk-media-worker"
```

Environment variables win over TOML, as everywhere else:
`BOOKCLERK_MEDIA_WORKERS`, `BOOKCLERK_MEDIA_ISOLATION` and
`BOOKCLERK_MEDIA_WORKER`.

### Isolation modes

`isolation` decides what happens when a confinement layer doesn't
engage:

- **`required`** (default): refuse the job. A host that can't sandbox
  shouldn't decode untrusted audio next to the master key.
- **`best-effort`**: apply what the platform supports and log the rest.
  For kernels without Landlock, where you accept unconfined codecs.
- **`off`**: run codecs in-process on a blocking thread, with no jail and
  no child process. For development only.

> [!IMPORTANT]
> `best-effort` only covers layers the **platform** can't enforce. A
> worker binary that was never installed is a packaging error, and both
> `required` and `best-effort` refuse the job rather than fall back to
> in-process work. Set `isolation = "off"` only if unconfined codecs are
> really what you want.

The pool decides at startup, not at the first acquire. A host that can't
honor its mode says so in the startup log, instead of failing every book
later.

### Changing settings while running

`[media]` reloads without a restart. A change builds a new pool, sends
new jobs to it, and lets the old one drain. Nothing is interrupted, and
nothing waits.

- Jobs already running keep the isolation they started with. A worker
  confines itself at spawn and can't change afterwards, so lowering
  `isolation` never loosens a running job, and raising it never confines
  one after the fact.
- While the old pool drains, total codec concurrency can briefly reach
  its in-flight jobs plus the new limit. That only shrinks, because the
  old pool admits no new jobs.

> [!NOTE]
> If you lower `media.workers` to relieve memory pressure, the new limit
> applies to new jobs at once, but jobs already encoding run to the end.

## Troubleshooting

### `refusing to run <job> unconfined: media isolation is required but unavailable`

Either the worker binary wasn't found (a configured path doesn't exist,
or nothing sits beside the host) or the platform has no way to confine
itself. The startup log says which, naming the folder it searched or the
backend that fell short.

- **The worker is missing.** Install it beside the host binary, or point
  `media.worker_bin` at it. `best-effort` doesn't help here; only `off`
  runs without the worker.
- **The platform can't confine.** If you accept unconfined codecs, drop to
  `best-effort`.

### `media worker (<job>) failed: worker exited with <status> before replying`

The worker died during the job. Its stderr goes to the host, so the
confinement summary and any codec output are in the daemon log just
above.

### `media worker (<job>) failed: worker reported success but exited with <status>`

The worker wrote a success reply and then died anyway, which it has no
legitimate way to do. It is usually the OOM killer, or a signal arriving
between the reply and the exit.

The job fails instead of being recorded, because a killed process can't
vouch for the file it just wrote. Check the host's memory pressure, and
lower `media.workers` if encoders are killed under load.

### A job can't find an input that is clearly on disk

The path wasn't in `read_paths()` for that kind of job, so the jail
denied it. A new input for a job must be added there too. The worker
checks declared inputs before it runs the codec, so the error names the
path instead of a bare `No such file` from inside a decoder.

### Everything is slow, with idle cores

`media.workers` is the cap. Acquire **batches** are also run one at a
time, by the job queue's `network` class (concurrency 1 by default) and
the daemon's `work_lock`. The pool limits codec work within one acquire,
not across acquires. See [Jobs](../internals/jobs.md).

## How the boundary works

One job is one process. Workers are never reused: confining a file system
can't be undone and applies to the whole process, so a long-lived worker
would need a jail wide enough for every job it might get. Media jobs take
seconds to minutes, so a few milliseconds to spawn a process is nothing
next to a tight per-job allowlist.

1. The host writes a JSON `MediaJob` to the worker's stdin, and closes it.
2. The worker builds its policy from the job and confines itself. On
   Windows, `bookclerk-jail` applied that policy at spawn instead.
3. It runs the codec and writes a JSON reply to stdout.

Nothing configures the jail apart from the request, so a job can never
get more than it declared.

### By platform

| Platform | Backend | File system | Syscalls | Network | Memory, CPU, processes |
| --- | --- | --- | --- | --- | --- |
| Linux | Landlock and seccomp-bpf | Allowlist, ABI-probed | Deny list | No IP sockets | cgroup v2, only when the spec sets limits (the media spec doesn't) |
| macOS | Seatbelt (`sandbox_init`) | Deny-by-default SBPL profile | Not applicable | Denied in the profile | Not supported (file system and network only) |
| Windows | AppContainer | At spawn only | Not applicable | No network capabilities | Job Object (media defaults: 2 GiB, 64 processes, CPU uncapped) |

**macOS** has no seccomp. Seatbelt gates classes of operations, not
syscall numbers, so the syscall layer reports "not applicable" rather
than a gap. The `(deny default)` profile already refuses `exec` and
anything not listed.

**Windows can't confine a running process**; AppContainer isolation is
granted at `CreateProcess`. So on Windows the pool launches the worker
through `bookclerk-jail`, the launcher plugins use:

- The launcher applies a **per-job** AppContainer profile and an ACL
  allowlist built from the job's read and write paths. Bookclerk never
  changes DACLs under protected, OS-managed folders.
- The worker joins a kill-on-close Job Object before it runs, with higher
  memory and process limits than plugins get.
- The worker skips confining itself and relies on that outer jail.
- Missing allowlist paths fail closed in the launcher, before the worker
  starts.

> [!WARNING]
> Ship `bookclerk-jail` beside the worker. With the default
> `isolation = "required"`, a missing jail refuses media work instead of
> decoding unconfined.

### What each job may touch

| Job | Reads | Writes |
| --- | --- | --- |
| `encode_mp3` | The input file | The output's folder |
| `remux_trimmed` | The input file | The output's folder |
| `fixup` | The input file and cover art | The output's folder |
| `package_m4b` | Every part | The output's folder |
| `align_chapters` | The input file | Nothing |

Write grants are folders, not single files, because codecs stage output
in temporary files beside the destination. Packaging can't know the final
sample table until it has read every part, so it first spills the joined
AAC data to a scratch file. That file goes in the output folder, not
`$TMPDIR` (which the jail doesn't grant), and is deleted when the job
ends.

### A clean environment

The worker starts with a scrubbed environment. It gets its job over stdin
and needs nothing from Bookclerk's configuration, so `BOOKCLERK_*`
variables, operator tokens and cloud credentials are dropped at spawn.
It keeps only locale, time zone, `RUST_BACKTRACE`, and the Windows
variables the loader and AppContainer need (such as `SystemRoot`, `PATH`
and `LOCALAPPDATA`). Otherwise a compromised codec could read the host's settings
from its own environment.

### Symbolic links

A symbolic link is granted at its **resolved target**, because that is
the inode the kernel checks. Declaring a link grants whatever it points
at. The host builds every job path from its own cache and output roots,
so nothing untrusted chooses them. A destination plugin that builds
output paths from user input should canonicalize them first.

## What this jail doesn't cover

### DRM decryption stays in the store plugin

DRM decryption also parses ISO-BMFF from outside sources, but it doesn't
run in a media worker. Audible's Adrm and Widevine CENC decryption run on
a blocking thread inside whichever process hosts the store plugin.

Two reasons keep it there:

- **Keys.** Decryption needs each title's content key, and the host is
  built never to hold one: `fetch_title` decrypts inside the store plugin
  and returns plaintext paths. Decrypting in the pool would mean writing
  content keys to a worker's stdin, trading a parser boundary for a
  key-handling one.
- **Distribution.** The decryption code lives in a plugin that can be left
  out of a build or package, which some regions may require of anyone
  shipping the core binaries. Moving it into `bookclerk-media` would
  remove that option.

### Brand trimming follows the same line

Where Audible's brand intro and outro sit is arithmetic on
`chapter_info`. `bookclerk_media::brand_trim_range` computes the window,
and the plugin applies it during the decryption pass it already makes. So
the branding never reaches the disk, and no second copy of the file is
made. Chapter splitting works on clear media, so it is an ordinary
`remux_trimmed` job in the pool.

### The shared MP4 code

The ISO-BMFF code underneath is shared, without moving that line.
`bookclerk-mp4` owns the parser, the sample table and the progressive
remuxer for both callers. What differs is the `SampleTransform` each
passes in:

- `bookclerk-media` passes `CopySamples`, which passes data through
  untouched.
- The Audible plugin passes a decryptor that holds the content key and
  CENC metadata. The remuxer never sees either.

No cipher, key or license code links into the shared crate, so leaving
the plugin out of a build still leaves out every part of decryption. See
[shipping without a store](../internals/guest-jail.md#shipping-without-a-store),
a build both hosts support and CI checks.

Sharing also made both muxers faster:

- Both used to clone the whole sample table to trim it, once in the
  plugin and once in the host. The shared remuxer selects by index, so a
  trim no longer allocates a second table.
- Both used to copy samples one syscall at a time: a `seek`, a `read` and
  a `write` per AAC frame of a few hundred bytes, a couple of million
  times per book. `SampleReader` buffers reads and tracks position, so
  samples in track order hit the buffer instead of seeking, and the
  writers are buffered to match.

Measured with `cargo run --release -p bookclerk-mp4 --example bench_remux`,
a copy remux of a 3-hour book went from about 700 ms to 400 ms. The same
buffering covers the packaging muxer and the plugin's fragmented-DASH
path.

### The plugin jail covers decryption

Confining the store plugin's process covers the decryption parser, and
everything else a store does. First-party stores ship as plugin binaries
as well as in-process adapters, so one jail reaches Audible, Libro.fm,
Chirp and GraphicAudio together. Hosts load those stores as jailed
external plugins by default, so decryption runs in the plugin's jail, not
in `bookclerkd`. See [The guest jail](../internals/guest-jail.md#the-guest-jail).

## Next steps

- [Architecture](../internals/architecture.md): where the pool sits in
  the acquire pipeline.
- [The guest jail](../internals/guest-jail.md#the-guest-jail): the other
  confined tier, for plugin processes. It lives longer, grants more, and
  is applied by a launcher.
- [Configuration](configuration.md): every `config.toml` setting.
- [Running Bookclerk](operations.md): running `bookclerkd`.
