# @bookclerk/plugin-sdk

TypeScript guest SDK for Bookclerk plugins (`api_version = 3`).
Workerd isolates host the author classes. Native guests use Rust `serve` /
`PluginWorker`.

| Import | Runtime |
| --- | --- |
| `@bookclerk/plugin-sdk/workerd` | `BookclerkEntrypoint` + named `*Entrypoint` bases (extend `WorkerEntrypoint`) |
| `@bookclerk/plugin-sdk` | ABI types, codecs, and the same workerd author classes |

Depend on the package (npm / `file:` / workspace); do **not** import a
relative embed path. `bookclerk-workerd` injects
`@bookclerk/plugin-sdk/workerd` into the isolate under that exact module
name.

```json
{
  "dependencies": {
    "@bookclerk/plugin-sdk": "file:../../packages/plugin-sdk"
  }
}
```

```bash
npx bookclerk-plugin check .
npx bookclerk-plugin types .            # writes bookclerk-configuration.d.ts
npx bookclerk-plugin package --out dist/plugins .
npx bookclerk-plugin smoke .            # workerd: download pin, describe + health
```

The guide, with the author model, the triggers, the `env` bindings and the
author tools, is the
[TypeScript plugin SDK](https://bookclerk.app/docs/plugins/sdk/typescript)
page ([`docs/plugins/sdk/typescript/README.md`](../../docs/plugins/sdk/typescript/README.md)
in this repository). The package's reference follows it on the same page.

## Tests

```bash
npm run build
npm run lint
npm run test:tools            # check / fmt fixtures
npm run test:db-value         # DbValue goldens
npm run test:plugin-migrations
npm run test:entrypoints      # event/job translation, env bindings, named dispatch
```

## Isolate embed

`embed/bookclerk_plugin.js` is the module `bookclerk-workerd` injects into
every plugin isolate as `@bookclerk/plugin-sdk/workerd`. It is **generated**:
`npm run build` compiles `src/` with `tsc` and bundles `dist/workerd.js` into
that single file with `scripts/build-embed.mjs` (esbuild; only
`cloudflare:workers` stays external). Edit `src/plugin.ts` and friends, never
the embed. The bundle is committed; CI fails when it is stale, and
`scripts/gen-plugin-abi.py --check` / `scripts/sync-workerd-pin.py --check`
keep the Rust and Python mirrors byte-identical. See
[`docs/internals/workerd-bridge.md`](../../docs/internals/workerd-bridge.md) for the wire the
launcher and the embedded adapter speak.
