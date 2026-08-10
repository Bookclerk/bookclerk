# Echo Integration (workerd TypeScript)

Greenfield Workers RPC guest that **extends `BookclerkPlugin`** from
`@bookclerk/plugin-sdk/workerd` (not bare `WorkerEntrypoint`). Id:
`echo-workerd-ts`. Isolation: `bookclerk-jail` + `bookclerk-workerd` + pinned
Cloudflare `workerd`.

Authoring: `src/index.ts` and `modules/index.js` both use

```ts
import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
```

(`package.json` depends on `@bookclerk/plugin-sdk`; the isolate import is
injected by `bookclerk-workerd`). Native Node guests use
`@bookclerk/plugin-sdk/native` instead.

See [docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md).

```bash
cd packages/plugin-sdk && npm ci && npm run build && npm run check-schema
cd ../../examples/plugins-echo-workerd-ts && npm ci && npm run typecheck
```

Install layout: `plugin.toml` + `modules/` under
`$BOOKCLERK_FILES_DIR/plugins/echo-workerd-ts/` (or staged artifacts).

`cargo build-app --platform` / `cargo dev` / `cargo ensure-workerd` fetch the
pinned Cloudflare `workerd` beside `target/<profile>/bookclerk-workerd`. There
is no JS-less shim — workerd guests require that binary. Override path with
`BOOKCLERK_WORKERD_BIN` if needed.

Out-of-tree author smoke (no Rust `bookclerk-workerd` binary required):

```bash
npx bookclerk-plugin smoke .   # from packages/plugin-sdk after npm ci && npm run build
```

Sibling examples: `plugins-echo-workerd-python`, `plugins-echo-workerd-rust`,
`plugins-echo-native-*`.
