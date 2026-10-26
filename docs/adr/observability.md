# ADR: Logging and tracing

- **Status:** Accepted; partly implemented (see [Status of the work](#status-of-the-work))
- **Date:** 2026-10-24
- **Related:** [Diagnostics](../server/diagnostics.md),
  [Operations](../server/operations.md)

## Context

Bookclerk runs as a daemon, a CLI, a confined media worker and jailed plugin
guests, often on a machine the maintainers never see. When something goes
wrong, the evidence is whatever the process wrote down. Operators want quiet
logs they can read or feed to a log aggregator. Diagnostics reports want as
much detail as possible, but only about the moments just before a failure.
These two audiences pull in opposite directions on how much to record.

## Decision

1. **Trace everything, show what is asked for.** Code is instrumented
   throughout in the OpenTelemetry style: spans around units of work, with
   trace and span ids, and structured fields on events. Standard log levels
   (ERROR, WARN, INFO, DEBUG, TRACE) decide how much each output shows.
2. **Diagnostics always keep TRACE.** The in-memory ring buffer behind
   [diagnostics reports](../server/diagnostics.md) records every level,
   whatever level the operator chose for their own outputs. A report then
   carries the detail that led up to a crash or an error burst, and nothing
   is uploaded unless the operator turned sharing on.
3. **Each output has its own level.** stderr and the OS log facility
   (journald, macOS `os_log`, Windows Event Log) follow `BOOKCLERK_LOG` /
   `RUST_LOG`. The daemon defaults to `bookclerk=info,warn`; the CLI is quiet
   unless asked (`-v`, `-vv`, `-vvv`). Changing them never changes what the
   ring keeps.
4. **Readable by default, structured on request.** The CLI writes plain,
   syslog-style lines and offers a JSON switch. The daemon writes JSON
   by default (`daemon.json_logs = true`), because it usually runs under a
   service manager or a log aggregator.
5. **Build on `tracing`.** The `tracing` crate stays the instrumentation API.
   Trace ids use the W3C trace-context shape, so an OTLP exporter can be
   added later without re-instrumenting the code.
6. **Redaction applies to every output.** The same redaction that covers
   stderr and the OS facility covers the ring and uploads (see
   [Diagnostics → Redaction](../server/diagnostics.md#redaction--privacy)).

## Status of the work

In place:

- The ring buffer keeps every level through TRACE, while stderr and the OS
  facility follow `BOOKCLERK_LOG` / `RUST_LOG`.
- The daemon writes JSON by default. The CLI writes `tracing`'s text format
  and is silent unless asked (`-v`).
- Redaction covers every sink, and uploads are abandoned if a registered
  secret remains.
- Domain events carry correlation and causation ids from the invocation that
  raised them.

Not yet done:

- Spans and trace or span ids on log events. Today the ring records events
  only (time, level, target, message, fields), and the code has almost no
  spans.
- Syslog-style lines and a JSON log format switch for the CLI.
- Changing the level of a running process.
- An OTLP exporter.
- Reports from the media worker and plugin guests. Today only `bookclerkd` and
  the CLI send reports.

## Consequences

- Operators' outputs stay at the level they chose, however much the code
  records. The ring is bounded (`ring_buffer_capacity`, default 200 events),
  so its memory is bounded too.
- TRACE is not free: the ring enables every callsite, so each event is
  built and redacted whatever level the outputs show. Keep TRACE fields
  small, and out of per-sample or per-chunk loops.
- Every new event field is a potential leak. New secret paths must register
  with redaction, as the project invariants already require.
- Reports become more useful as spans land, because triage can follow one
  unit of work across the daemon, the media worker and guests.
