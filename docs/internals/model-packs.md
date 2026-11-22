# Embedding model packs

This page describes how a text-embedding model is stored on disk for the
first-party `embedder` plugin, how the plugin runs it, and how the plugin
was checked against the published models. The decision behind it is in
[Models run in plugins](../adr/ai-models.md), and the choice of models in
[Embedding model benchmarks](embedding-benchmarks.md).

> [!NOTE]
> The `embedder` plugin ships with Bookclerk and is on by default, but no
> model is delivered with it yet. Until model delivery lands, the plugin
> lists no models, and Discover keeps ranking with hashed words.

## Why a model is cut into packs

Most of a multilingual embedding model is its token table: one row of
numbers for each of about 256,000 tokens. bekko-a8m's published file is
130 MB, of which the transformer is 31 MB. A library that holds books in
two languages never reads most of those rows.

So the table is taken out of the graph and stored as row packs:

- every text needs the **base rows**: the special tokens, single
  characters and byte pieces;
- each language adds a **language row pack** with the tokens its text
  uses.

A node stores the graph, the tokenizer, the base rows and the row packs
of the languages it was given.

## The cut graph

The pack builder finds every vocabulary-sized table the graph reads with
`input_ids` and turns it into a graph input. Nothing else in the graph
changes. At run time the plugin:

1. tokenizes the text with the full vocabulary, so token ids are the
   published model's own;
2. looks up each token's row in the loaded row packs;
3. passes the rows as the table inputs, one row per token in order, and
   `input_ids` as `0, 1, 2, …`, so the graph's own lookup picks row *n*
   for token *n*;
4. pools the output as the description says and scales the vector to
   length 1.

The same code serves both chosen models, with no model-specific Rust:

| Model | Lookup node | One row | Row bytes | Output |
| --- | --- | --- | --- | --- |
| bekko-a8m | `Gather` | 384 signed bytes and 1 float scale | 388 | one vector per token, averaged |
| Harrier 270m (4-bit) | `GatherBlockQuantized` | 320 packed bytes, 20 float scales and 10 zero points | 410 | the text's vector |

## A model directory

Models live in the plugin's data directory, one directory per model id:

```text
models/
  bekko-a8m/
    model.json         the model description
    core.onnx          the cut graph
    tokenizer.spm.fb   the SentencePiece Lite model, whole vocabulary
    base.rows          rows every text needs
    en.rows            rows for English (one file per language)
```

`model.json` (`PackManifest` in `bookclerk-model-pack`) holds:

- the layout version, the model id, the vector size and the token limit;
- the graph's input and output names and how its output is pooled;
- the tokenizer's framing pieces (`<bos>`, `<eos>`), the pieces matched
  before tokenizing, and the text put before a document or a query;
- the tables, in the order a row concatenates them;
- the name, size and SHA-256 of every other file, including each
  language's row pack whether it is on disk or not.

A file name is 1 to 64 lowercase letters, digits, `.`, `_` and `-`,
starting with a letter or digit, so no name can leave the directory.
Unknown fields and unknown layout versions are refused.

### Row packs

A row pack is one little-endian file:

| Bytes | Field |
| --- | --- |
| 8 | the magic `BCROWS1` and a zero byte |
| 4 | `count`: rows in the pack |
| 4 | `row_bytes`: bytes per row |
| 4 × `count` | token ids, strictly ascending |
| `row_bytes` × `count` | rows, in the order of the ids |

One row is the token's row of every table, concatenated. The reader
checks the whole layout once; after that a lookup is a binary search.

### Revision

Vectors made by different models, or by the same model with different
languages loaded, are never compared. The plugin reports a **revision**
with every reply: the first 32 hexadecimal characters of a SHA-256 over
the digest of `model.json` and the sorted language tags whose row packs
are loaded. The host stores it with each vector.

## What the plugin does

- **`models`** lists the directories whose `model.json` is valid. It
  reads only that file, so it is cheap.
- **`run`** loads the model on first use: it checks every file against
  the size and SHA-256 in `model.json`, then starts one ONNX Runtime
  session with one thread. A model stays in memory until the plugin
  sleeps, and is loaded again when the files on disk give another
  revision.
- **`health`** reports whether the ONNX Runtime library is beside the
  plugin, how many models are installed, and any directory it skipped.

### Text in a language that is not loaded

A token with no row in the loaded packs is spelled with single-character
pieces, which the base rows always hold. The vector is still returned,
and the text's index is listed in the reply's `fallbacks`, so the caller
knows the vector is weaker and can embed the text again once the
language is added.

## ONNX Runtime and the tokenizer

- **ONNX Runtime** is the upstream build, pinned by version and SHA-256
  in `crates/bookclerk-onnxruntime/onnxruntime-pin.json` for Linux
  x86-64 and arm64, macOS arm64 and Windows x86-64. `cargo
  ensure-onnxruntime` downloads it; staging and packaging put the shared
  library beside the plugin's executable, and the plugin loads it from
  there. There is no build for Intel macOS, so the plugin reports itself
  unhealthy there and serves no model.
- **SentencePiece Lite** is compiled from the sources vendored under
  `third_party/sentencepiece-lite/`
  (`scripts/vendor-sentencepiece-lite.sh` refreshes them).

Both are C++ and run only inside the plugin's jail, which has no network
grant. Neither is linked into a host process.

## How it was checked

### Against the published models

Each model was cut and packed, then 69 texts were embedded twice: once by
the plugin's code from the pack, and once by the published ONNX file with
the published tokenizer. The texts were 60 Open Library descriptions, 7
multilingual and odd texts (Japanese, mixed scripts, emoji, blank lines)
and 2 queries.

| Model | Largest difference in any number | Texts equal to 1e-6 |
| --- | --- | --- |
| bekko-a8m | 1.8e-7 | 69 of 69 |
| Harrier 270m (4-bit) | 3.1e-7 | 69 of 69 |

With half the vocabulary removed from the packs, all 69 texts were
reported in `fallbacks` and the lowest cosine similarity to the true
vector was 0.57, which is why a fallback vector is flagged and not
trusted.

What a node stores before any language is added:

| Model | Cut graph | Tokenizer | Base rows | Total |
| --- | --- | --- | --- | --- |
| bekko-a8m | 30.8 MB | 9.5 MB | 8.3 MB (21,225 tokens) | 48.6 MB |
| Harrier 270m (4-bit) | 98.2 MB | 10.5 MB | 10.7 MB (25,895 tokens) | 119 MB |

These runs used the pack tools on a development machine. The workflow
that builds, checks and signs packs in CI comes with model delivery.

### In the test suite

- The plugin's tests run a tiny model (a few kilobytes, the same inputs
  as a real one) through the pinned ONNX Runtime and compare every
  vector with numbers worked out apart from the plugin. They cover
  language packs, fallbacks, long texts, changed files and reloads.
- The tokenizer's tests compare token ids with ids the full
  SentencePiece library recorded.
- The staged-plugin suite installs the tiny model and runs the plugin
  inside its jail on Linux.
- The `model_pack` fuzz target checks the `model.json` and row-pack
  readers against a layout check written apart from them (see
  [Fuzzing](../contributing/fuzzing.md)).

## Next steps

- [Models run in plugins](../adr/ai-models.md) for the `ai` entrypoint
  and the `AI` binding.
- [The guest jail](guest-jail.md) for how the plugin is confined.
