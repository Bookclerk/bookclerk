# ABI wire golden fixtures

JSON samples for host↔guest Workers RPC payloads. Keys are **camelCase** to
match `schema/abi.json` `$defs` and the TS / Python SDKs.

Consumed by:

- Rust: `bookclerk-plugin-abi` (`src/wire_fixtures.rs`)
- Python: `packages/plugin-sdk-python/tests/test_wire_casing.py`
- TypeScript: `packages/plugin-sdk/scripts/check-wire-fixtures.mjs`
- `scripts/gen-plugin-abi.py --check`
