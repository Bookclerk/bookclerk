# Echo Integration (id `echo_native_node`)

This example now validates **workerd** hosting (`runtime = "workerd"`,
`api_version = 2`), not a Node Cap'n Proto stack. The guest class lives in
[`modules/index.js`](modules/index.js) and matches
[`plugins-echo-workerd-ts`](../plugins-echo-workerd-ts/).

```bash
cargo ensure-workerd
cargo stage-plugins --examples --skip-build
```

Historical Node SEA sketches under `scripts/build-sea.mjs` and
`.github/workflows/package.yml` are **not** the guest entry. Keep them only as
publisher notes; they do not speak the product ABI.

Sibling examples: `plugins-echo-native-rust`, `plugins-echo-native-python`,
`plugins-echo-workerd-*`.
