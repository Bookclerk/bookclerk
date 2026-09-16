# Echo Integration (workerd TypeScript)

Greenfield Workers RPC guest that **extends `BookclerkPlugin`** from
`@bookclerk/plugin-sdk/workerd` (not bare `WorkerEntrypoint`). Id:
`echo_workerd_ts`. Isolation: `bookclerk-jail` + `bookclerk-workerd` + pinned
Cloudflare `workerd`.

Authoring: `src/index.ts` and `modules/index.js` both use

```ts
import { BookclerkPlugin, Integration } from "@bookclerk/plugin-sdk/workerd";

export default class EchoPlugin extends BookclerkPlugin {
  async describe() {
    return {
      apiVersion: 2,
      id: "echo_workerd_ts",
      kind: "integration",
      displayName: "Echo Integration (workerd TypeScript)",
      rpcFeatures: ["rpc.scalarLimits"],
      scalarLimits: {
        maxScalarBytes: 262144,
        maxStreamWindowBytes: 1048576,
        maxListPage: 256,
      },
      supportedRoles: ["integration"],
    };
  }
  integration() {
    return new EchoIntegration(this.env);
  }
}
```

(`package.json` depends on `@bookclerk/plugin-sdk`; the isolate import is
injected by `bookclerk-workerd`). The `echo_native_node` example is also a
workerd guest (same JS class ABI).

See [docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md).

```bash
cd packages/plugin-sdk && npm ci && npm run build
cd ../../examples/plugins-echo-workerd-ts && npm ci && npm run typecheck
```

Install layout: `plugin.toml` + `modules/` under
`$BOOKCLERK_FILES_DIR/plugins/echo_workerd_ts/` (or staged artifacts).

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
