# ABI wire golden fixtures

JSON samples for the JSON payloads carried inside `Text` fields of the
Cap'n Proto ABI. Keys are **camelCase** to match the "JSON payload contracts"
section of `schema/plugin.capnp` and the TS / Python SDKs.

Consumed by:

- Rust: `bookclerk-plugin-abi` (`src/wire_fixtures.rs`)
- Python: `packages/plugin-sdk-python/tests/test_wire_casing.py`
- TypeScript: `packages/plugin-sdk/scripts/check-wire-fixtures.mjs`
- `scripts/gen-plugin-abi.py --check`
