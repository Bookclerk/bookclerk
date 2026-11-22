# ADR: Models run in plugins and are called through one `AI` binding

- **Status:** Accepted; the entrypoint, the binding, the router and the first-party model plugin are built, model delivery is not
- **Date:** 2026-11-21
- **Related:** [Plugin Workers RPC on workerd](plugin-workers-rpc-workerd.md),
  [Plugin capabilities](plugin-capabilities-v3.md),
  [Event delivery model](event-delivery-model.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md)

## Context

Discover ranks books by taste, and semantic search needs to match a
query to books by meaning. Both need **embeddings**: a model turns a
text into a vector, and texts with similar meaning get nearby vectors.

Today the host embeds text itself. The default is a hash of the words,
which carries no meaning. An optional build feature runs an English
model inside the host process through ONNX Runtime, a C++ library, on
descriptions that come from stores.

The [embedding model benchmarks](../internals/embedding-benchmarks.md)
show that a small multilingual transformer, trimmed to the library's
languages, gives useful taste matching in about 220 MB and 30 ms a
description on one core. They also show that the engines without C++
are 3 to 5 times slower for the same model.

More model work is planned after embeddings: transcribing audiobooks,
embedding those transcripts, and matching audio. Plugin authors will
want models too, for example a metadata plugin that classifies genres.
Each of these should not need its own host feature.

Bookclerk's rules for untrusted work already apply here. Nothing that
parses untrusted bytes runs in the host process, a missing helper fails
closed, and a guest gets only the capabilities it declared and the
operator approved.

## Decision

### Models are served by plugins

A plugin serves models through a new **`ai` entrypoint**. It lists the
models it serves, each with a **task**, and runs one model on one input:

- `models()` returns each model's name, task, revision digest and the
  facts a caller needs for that task. For text embeddings these are the
  vector length, the token limit and the languages loaded.
- `run(model, input)` runs the model. The input and output have one
  member per task, and exactly one is set.

**Text embeddings** is the first task. Its input is a batch of texts,
each marked as a document or a query, and its output is one vector per
text plus the model revision that made them. New tasks are new members,
so the entrypoint does not change when one is added.

The host never loads a model. With no plugin serving text embeddings,
Discover keeps ranking with the hash embedder, a few lines of arithmetic
over the words of a text that carries no meaning, and semantic search
reports that it is unavailable. Hash vectors are stored under their own
model name and are never compared with a model's vectors. The in-process
ONNX feature is removed.

### One binding for every caller

Plugins call models through an **`AI` binding**, shaped like the one on
Cloudflare Workers:

```ts
const out = await env.AI.run("@embedder/bekko-a8m", {
  text: ["A detective in Victorian London"],
});
```

- The host routes the call to the plugin deployed on the same node that
  serves the named model. The host's own features use the same router.
- A model's name is `@<plugin id>/<model id>`. A task alias,
  `@library/text-embeddings`, resolves to the library's chosen model for
  the task, so a caller need not know which is installed.
- Input and output shapes follow Cloudflare's for the same task where
  one exists, so Workers code carries over.
- A plugin declares the tasks it calls in `plugin.toml`, and the
  operator approves that grant like any other capability. A plugin
  without the grant has no `AI` binding.
- A call has a deadline (30 seconds) and counts against the calling
  plugin's budget (240 input items a minute). Work on many items, such as
  embedding a whole library, is a job, not a loop of calls.

### The first-party model plugin

Bookclerk ships one model plugin for text embeddings:

- It is a native guest, spawned through `bookclerk-workerd` and jailed
  with no network grant.
- It runs ONNX Runtime and the SentencePiece Lite tokenizer, both C++.
  ONNX Runtime is the upstream build, pinned by digest; the tokenizer is
  compiled from pinned sources in the repository. The jail is what makes
  that acceptable: the libraries never enter the host.
- It sleeps when idle, like every plugin, so its memory is held only
  while it works.
- It serves the models found in its data directory. Which model and
  languages a library uses is chosen in the host, which delivers the
  files.

Models are delivered as a cut graph, a tokenizer, the rows every text
needs and one row pack per language
([Embedding model packs](../internals/model-packs.md)). A workflow builds
them from pinned upstream revisions, checks token ids against the
reference tokenizer, and signs what it publishes. The host downloads and
verifies packs against pinned digests, and the guest checks every file
against the model description before it loads it.

### Vectors are stored once and searched in memory

- The `embeddings` table is the one durable copy. Each row records the
  model revision, the language packs, a hash of the input and what was
  embedded.
- A vector is computed once per model and input, and shared by everyone
  who needs it.
- Each node keeps the vectors it searches in a flat in-memory index,
  rebuilt from the table and kept current from events. No engine
  specific vector type or vector database is used.
- Vectors from different model revisions are never compared. Changing
  the model re-embeds in a background job while the old vectors keep
  serving.

## Consequences

- Any plugin can use a model with one call, and anyone can ship a model
  plugin, including one that calls a hosted model through an approved
  network grant.
- Transcription, audio matching and other tasks arrive as new tasks and
  new plugins, without new host features.
- A library without a model plugin has no semantic search, and its
  Discover ranking falls back to hashed words. Settings show which of
  the two is in use.
- The reply of a model plugin is untrusted input to the host. It ships
  with a fuzz target that checks vector count, length and finite values
  against an independent oracle.
- A model call costs CPU on the node that runs the plugin. Budgets and
  deadlines bound it, and the scheduler's capacity figures have to count
  it.
- Two pinned C++ libraries join the build of one plugin. They are
  tracked for advisories like any dependency.
- The pure-Rust engines were measured and rejected for now on speed.
  Because the entrypoint hides the engine, a later plugin can use one
  without any change to callers.
