# Echo Fetch (workerd)

Reference workerd guest that requests **outbound** network with allowlist
`*.example.com`, embeds `assets/logo.svg`, and probes
`https://www.example.com/` from `diagnose` / `cliInvoke fetch-example`.

Id: `echo_workerd_fetch`. Isolation: `bookclerk-jail` + `bookclerk-workerd` +
pinned Cloudflare `workerd`.

**Allowlist probe semantics:** success means egress returned a `Response`
(the hop was allowed). HTTP status from example.com is **best-effort** and must
not fail the probe — origins may return any status while still proving the
domain grant works. A thrown/`TypeError` (or egress deny) fails the probe.

```bash
cd packages/plugin-sdk && npm ci && npm run build
cd ../../examples/plugins-echo-workerd-fetch && npm ci && npm run typecheck
```

Stage with other examples:

```bash
cargo stage-plugins --examples --skip-build
```

Sibling: [`plugins-echo-workerd-ts`](../plugins-echo-workerd-ts/) (deny network).
