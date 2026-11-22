# Embedding model packs

This page describes how a text-embedding model is stored on disk for the
first-party `embedder` plugin, how the plugin runs it, and how the plugin
was checked against the published models. The decision behind it is in
[Models run in plugins](../adr/ai-models.md), and the choice of models in
[Embedding model benchmarks](embedding-benchmarks.md).

> [!NOTE]
> The `embedder` plugin ships with Bookclerk and is on by default, and
> the packs are built and published, but the host does not download them
> yet. Until it does, the plugin lists no models, and Discover keeps
> ranking with hashed words.

## Why the token table is taken out of the graph

Most of a multilingual embedding model is its token table: one row of
numbers for each of about 256,000 tokens. bekko-a8m's published file is
130 MB, of which the transformer is 31 MB.

The table is taken out of the graph and stored as **row packs**, files of
token rows that the plugin looks up itself:

- the **base rows** are loaded for every text;
- a **language row pack** adds the rows of one language, and is loaded
  only when it is on disk.

The packs published today put every token's row in the base rows and have
no language packs, because splitting by language saved little and cost
accuracy ([measured below](#why-the-published-packs-hold-the-whole-vocabulary)).
The layout and the plugin still read language packs.

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
    base.rows          rows every text needs (published packs: every row)
```

A pack split by language also has one `<language>.rows` file for each
language it was given.

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

### A token with no row

A published pack has a row for every token, so this does not happen with
one. With a pack split by language, a token with no row in the loaded
packs is spelled with single-character pieces, which the base rows always
hold. The vector is still returned, and the text's index is listed in the
reply's `fallbacks`, so the caller knows the vector is weaker and can
embed the text again once the language is added.

### Spaces at the start of a text

bekko's tokenizer puts a word boundary before a text and Gemma's
(Harrier's) does not. The plugin asks the tokenizer which kind it is when
it loads, and drops a leading space only for the first kind, so both give
the published tokenizer's ids for a text that starts with a space.

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

## How packs are built

`scripts/model_packs.py` builds a pack from a published model. Everything
it reads is pinned in `scripts/model-packs/models.json`: the Hugging Face
repository and commit, and the size and SHA-256 of each file.

| Model id | Published model | License |
| --- | --- | --- |
| `bekko-a8m` | `hotchpotch/bekko-embedding-v1-a8m`, `onnx/model.onnx` | MIT |
| `harrier-270m-q4` | `onnx-community/harrier-oss-v1-270m-ONNX`, `onnx/model_q4.onnx` (Microsoft's Harrier 270m, 4-bit) | MIT |

A build has four steps:

1. **`fetch`** downloads the pinned files and refuses any whose size or
   digest differs.
2. **`build`** cuts the graph, writes every token's row to `base.rows`,
   converts `tokenizer.json` to a SentencePiece model and then, with
   upstream's `spm_to_fb`, to the Lite file, and writes `model.json` and
   the OCI manifest.
3. **`check`** tokenizes and embeds every text in
   `scripts/model-packs/check-texts.json` twice: with the published
   tokenizer and ONNX graph, and with the plugin's own code from the pack
   (its `tokens` and `embed` examples). Token ids must be equal, no text
   may fall back to single characters, and no number of any vector may
   differ by more than 0.00001.
4. **`pin`** compares the SHA-256 of the built `model.json` and of the
   OCI manifest with the ones in `models.json`, and fails when they
   differ.

The build is reproducible: the same pinned inputs give the same bytes, so
the digests in `models.json` are reviewed with the change that produces
them. `spm_to_fb` is built from the google/sentencepiece commit the
vendored runtime comes from, by `scripts/model-packs/build-spm-to-fb.sh`,
which fetches every other source that build needs by a pinned commit
itself and lets CMake fetch nothing.

To build a pack on a development machine (Linux):

```bash
python3 -m venv target/model-packs/venv
target/model-packs/venv/bin/pip install --require-hashes -r scripts/model-packs/requirements.txt
scripts/model-packs/build-spm-to-fb.sh target/model-packs/tools
cargo build --release -p bookclerk-plugin-ai-embedder --example embed --example tokens
cargo ensure-onnxruntime

py=target/model-packs/venv/bin/python
$py scripts/model_packs.py fetch --model bekko-a8m
$py scripts/model_packs.py build --model bekko-a8m --spm-to-fb target/model-packs/tools/spm_to_fb
$py scripts/model_packs.py check --model bekko-a8m --examples target/release/examples \
    --onnxruntime target/onnxruntime-1.32.0/libonnxruntime.so
$py scripts/model_packs.py pin --model bekko-a8m
```

`pin --write` records new digests after a deliberate change to a model,
the builder or the layout.

### How a tokenizer is converted

The published models ship a `tokenizer.json`. The builder writes the same
vocabulary as a SentencePiece model:

- a piece's score is minus the rank of the first merge that makes it, so
  merges apply in the published order;
- byte pieces (`<0x41>`) and the unknown piece keep their kinds, and
  `<bos>`, `<eos>` and `<pad>` become control pieces, which text cannot
  spell;
- the tokenizer's other added tokens (newlines, HTML tags) are matched
  before anything else, as the published tokenizer does. For Harrier,
  SentencePiece does that itself. bekko's tokenizer also puts a space
  after each match, which SentencePiece cannot express, so the plugin
  matches them (`literal_pieces` in `model.json`).

## Where packs are published

[`.github/workflows/model-packs.yml`](../../.github/workflows/model-packs.yml)
runs the four steps for each model on every pull request that touches the
builder, the layout, the tokenizer or the plugin. On `main` it also
publishes each pack to the GitHub Container Registry as an OCI artifact:

```text
ghcr.io/bookclerk/models/<model id>@sha256:<manifest digest>
```

- The artifact has one layer per file, named by its
  `org.opencontainers.image.title` annotation, `model.json` first. A
  layer's digest is the file's SHA-256, the same one `model.json` lists.
- The manifest carries no time stamp, so a pack has one digest however
  often it is built. It is also tagged `sha256-<first 12 characters of
  the digest>`.
- `publish` measures the files again and refuses a build that is not the
  pinned one.
- Each artifact carries a build-provenance attestation, signed through
  Sigstore with the workflow's identity:

  ```bash
  gh attestation verify oci://ghcr.io/bookclerk/models/bekko-a8m@sha256:<digest> --owner bookclerk
  ```

The digests a node will trust are the ones in `models.json`, in the
repository it was built from. The attestation lets anyone else check
where an artifact came from.

## How a node gets a model

The plugin has no network, so the host downloads the model for it. Each
node does this for itself, in the background, the first time the plugin
starts there.

### What the plugin package pins

The plugin package carries its own `models.json`
(`ModelOffers` in `bookclerk-plugin-catalog`), beside `plugin.toml`:

```json
{
  "registry": "ghcr.io/bookclerk/models",
  "default": "bekko-a8m",
  "models": {
    "bekko-a8m": {
      "label": "Bekko (fast)",
      "summary": "Fast on any machine: about 30 ms per book description on one core.",
      "model_json": { "bytes": 6330, "sha256": "f826a998…" },
      "download_bytes": 140609206,
      "memory_bytes": 230686720
    }
  }
}
```

- `model_json` is the size and SHA-256 of the model's `model.json`. That
  file lists the size and SHA-256 of every other file, so one digest
  fixes the whole model.
- `download_bytes` and `memory_bytes` are what the picker shows.
  `memory_bytes` is the plugin's measured peak memory on the check texts
  (210 MiB for bekko-a8m, 372 MiB for Harrier), rounded up.
- A test holds this file to the builder's pins
  (`scripts/model-packs/models.json`) and to the values of the plugin's
  `model` setting, so the three cannot drift apart.

Only a platform plugin has models installed this way. The host downloads
with its own network, so a `models.json` in any other package is ignored
and logged.

### The download

The host asks the registry for an anonymous pull token, then fetches
blobs by digest. It never reads an OCI manifest: a blob's name is the
SHA-256 of a file, and the pins say which blobs to fetch.

1. Fetch the blob named by `model_json.sha256` and check its size and
   digest. Read it as a model description, and refuse one that names
   another model.
2. Fetch each file the description lists into `models/.incoming-<model id>`
   and check its size and SHA-256. A dropped connection resumes with a
   `Range` request; a file a stopped download already finished is kept.
3. When every file is there, move the directory to `models/<model id>`.
   The plugin never sees a partial model: a directory name that starts
   with a dot is not a model id.
4. Remove every other model directory. The model a library used before
   stays until the new one is in place, and the router keeps answering
   from it until then.

The token reply is the only thing parsed before a digest check. The
token goes into a request header, so it must be 1 to 8192 characters
from `A-Z a-z 0-9 . _ ~ + / = -`. The token is sent only to the registry
host, never to a redirect target.

A failed download is tried again after 1 minute, then 2, 4 and so on up
to an hour. The picker in Settings shows the state and the last error.

### Which model a library uses

The plugin's `model` setting names the model (`bekko-a8m` by default).
It is an instance setting like any other, so it is stored in the
database and every node that runs the plugin follows it:

```bash
bookclerk plugins config set embedder model harrier-270m-q4
```

**Settings → Plugins → Text embeddings** lists the models with their
download size and memory need, and where each stands on the answering
node. A model that needs more memory than the plugin may use on that
node (512 MiB by default) cannot be picked there.

The chosen model becomes the one `@library/text-embeddings` names once
it is installed. Vectors of two models are never compared, so features
that store vectors embed the library again after a change.

### Nodes that do not download

`[plugins.models]` in `config.toml` turns the download off or points it
at a mirror (see [Model downloads](../server/configuration.md#model-downloads)).
With `download = false` an operator can still place a model by hand:
copy the files into `plugin-state/<plugin fs-id>/data/models/<model id>/`
and restart. The host accepts a directory only when its `model.json` is
the pinned one and every listed file has the listed size, and the plugin
checks every file's SHA-256 before it loads the model.

The hosted demo never downloads: a visitor's container lives for
minutes.

A model takes 141 to 217 MB of the plugin's data directory, whose
budget is 512 MiB by default. While a library changes from one model to
the other, both are on disk.

## How it was checked

### Against the published models

Every build runs the `check` step. Its 64 texts are book descriptions and
search queries written for it in English, German, French, Spanish,
Italian, Portuguese and Japanese, with Russian, Arabic, Korean and Greek
sentences, and odd input: tabs and blank lines, HTML, emoji, a leading
space, mixed scripts and a text longer than the token limit.

| Model | Token ids equal | Largest difference in any number | Texts that fell back |
| --- | --- | --- | --- |
| bekko-a8m | 64 of 64 | 2.0e-7 | 0 |
| Harrier 270m (4-bit) | 64 of 64 | 3.9e-7 | 0 |

What a node stores for one model:

| Model | Cut graph | Tokenizer | Token rows | Total |
| --- | --- | --- | --- | --- |
| bekko-a8m | 30.8 MB | 9.5 MB | 100.4 MB (256,000 tokens) | 140.6 MB |
| Harrier 270m (4-bit) | 98.2 MB | 10.4 MB | 108.5 MB (262,144 tokens) | 217.2 MB |

### Why the published packs hold the whole vocabulary

The first design gave each language its own row pack, so a library would
store only its languages. It was measured on bekko-a8m before the packs
were built.

Tokens were counted over 4,000 Wikipedia articles per language (the
`wikimedia/wikipedia` dataset, November 2023), keeping a token for a
language when it appeared at least three times. Symbols, digits and
single characters went to the base rows (29,400 tokens, 11 MB).

| Language | Tokens in its pack | Size |
| --- | --- | --- |
| English | 80,900 | 31 MB |
| German | 65,700 | 25 MB |
| French | 62,500 | 24 MB |
| Spanish | 62,300 | 24 MB |
| Italian | 45,700 | 18 MB |
| Portuguese | 45,300 | 18 MB |
| Japanese | 27,400 | 11 MB |

Three results decided against it:

- **The packs overlap.** The seven packs add up to 151 MB but hold only
  146,000 different tokens (57 MB): languages written in Latin letters
  share most of their tokens. The whole vocabulary is 100 MB.
- **Real descriptions use words an encyclopedia does not.** Of 765 Open
  Library descriptions embedded with the English pack, 60% had at least
  one token with no row (32% when a token was kept after one appearance),
  from blurb words such as "riveting" and "unforgettable", from names,
  and from descriptions in another language. A vector from a text with
  half its rows missing had a cosine similarity as low as 0.57 to the
  true one.
- **The saving is small.** An English-only library would store about 85
  MB in place of 141 MB.

Loading the whole vocabulary costs memory, not time. Peak memory for 200
descriptions, model load included:

| Rows loaded | Peak memory | Time | Texts that fell back |
| --- | --- | --- | --- |
| Whole vocabulary | 211 MB | 7.0 s | 0 of 200 |
| Half the vocabulary | 167 MB | 16.3 s | 200 of 200 |
| Base rows only (single characters) | 125 MB | 20.8 s | 200 of 200 |

A text that falls back is slower as well as weaker, because a word
spelled with single characters is many more tokens.

The middle row is a stand-in, not a language pack: it holds half of the
token rows, not chosen for these texts, so every text fell back. It shows
what a fallback costs. A pack that held every token of a text would embed
it as fast as the whole vocabulary does.

With the whole vocabulary there is no language to choose, no text is
embedded again when a language is added, and a vector never depends on
which packs a node holds.

### In the test suite

- The builder's tests (`scripts/tests/test_model_packs.py`) cover the
  pins, the layout, the tokenizer conversion and the registry calls. The
  `model.json` and row pack they write are also read by
  `bookclerk-model-pack`'s tests, so the builder and the plugin cannot
  drift apart.
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
- The host's download is tested against a registry on loopback: a
  model is downloaded and verified, a dropped file resumes, a file or a
  description that is not the pinned one is refused, a failed download
  waits and reports why, and a new choice replaces the old model only
  once it is installed.
- The `model_fetch` fuzz target checks the token reply and `models.json`
  readers: an accepted token is safe in a header, and every URL an
  accepted list leads to is `https://` on the listed registry host.
- The staged-plugin suite has the host install the tiny model for the
  jailed `embedder` from a loopback registry, then has the
  `echo_workerd_ts` example run `@library/text-embeddings` through its
  `AI` binding and compares the vector with the model's arithmetic. That
  is two real plugins in two jails with the host routing between them.

## Next steps

- [Models run in plugins](../adr/ai-models.md) for the `ai` entrypoint
  and the `AI` binding.
- [The guest jail](guest-jail.md) for how the plugin is confined.
