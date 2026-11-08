# How it works

This shelf is for contributors who want to know how Bookclerk is built:
the crates, the job queue, the storage limits, and how every plugin is
confined. It assumes you can read Rust.

## Pages on this shelf

- [Architecture](architecture.md): how the pieces fit.
- [Discovery design](discovery.md): how suggestions are found and ranked.
- [Durable job queue](jobs.md): admission, leases and crash recovery.
- [Bounded storage](storage-bounds.md): size caps, streams, paged scans
  and checkpoints.
- [The guest jail](guest-jail.md): why plugins run as jailed processes,
  and how each platform confines them.
- [workerd bridge](workerd-bridge.md): the wire between the launcher and
  a plugin isolate.
- [Node API and the live socket](node-api.md): how hosts of one library
  talk, and how the web app hears about changes.
- [One TLS backend](tls.md): the one crypto provider every TLS connection
  uses, and where it is set.
- [Windows AppContainer](windows-appcontainer.md): confinement notes for
  Windows.
- [Rust crates](crates/README.md): the reference for every workspace
  crate, from its doc comments (generated).

## Next steps

- [Contribute](../contributing/README.md): build, test and send a change.
- [Design decisions](../adr/README.md): why things are the way they are.
