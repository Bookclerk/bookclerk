# @bookclerk/plugin-sdk

TypeScript guest SDK for Bookclerk plugins (`api_version = 2`).
Workerd isolates host the author class. Native guests use Rust `serve` /
`PluginRoot`.

| Import | Runtime |
| --- | --- |
| `@bookclerk/plugin-sdk/workerd` | `BookclerkPlugin` extends `WorkerEntrypoint` |
| `@bookclerk/plugin-sdk` | ABI types + workerd `BookclerkPlugin` |

Authors export the raw class:

```ts
import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
import type { PluginDescribe } from "@bookclerk/plugin-sdk/workerd";

export default class MyPlugin extends BookclerkPlugin {
  async describe(): Promise<PluginDescribe> {
    return {
      apiVersion: 2,
      id: "my_plugin",
      kind: "integration",
      rpcFeatures: [],
      scalarLimits: {
        maxScalarBytes: 262144,
        maxStreamWindowBytes: 1048576,
        maxListPage: 256,
      },
    };
  }
}
```

Depend on the package (npm / `file:` / workspace) — do **not** import a relative
embed path. `bookclerk-workerd` injects `@bookclerk/plugin-sdk/workerd` into the
isolate under that exact module name.

`package.json`:

```json
{
  "dependencies": {
    "@bookclerk/plugin-sdk": "file:../../packages/plugin-sdk"
  }
}
```

## Author tools

```bash
npx bookclerk-plugin check .
npx bookclerk-plugin fmt [--check] plugin.toml
npx bookclerk-plugin package --out dist/plugins .
npx bookclerk-plugin smoke .   # workerd plugins: download pin, describe + health
```

`smoke` does **not** need a built Rust `bookclerk-workerd` binary. It downloads
the pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override with
`BOOKCLERK_WORKERD_CACHE` / `BOOKCLERK_WORKERD_BIN`).
