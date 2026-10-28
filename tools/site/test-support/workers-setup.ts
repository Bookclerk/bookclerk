// Runs before the tests in the `workers` project (vitest.config.ts).
//
// Under `npm run coverage` the Workers Vitest integration defines a bare
// `window` (only `window.location`) for Vitest's V8 coverage, which the
// Workers runtime itself never has. elkjs, which beautiful-mermaid lays
// diagrams out with, then takes `window` for the global scope and fails on
// `window.Error`. Resolving the stub's other names through the real global
// scope gives the libraries what they expect and leaves the stub's own
// `location` for the coverage code.

const stub = (globalThis as { window?: object }).window;
if (stub !== undefined && !("Error" in stub)) Object.setPrototypeOf(stub, globalThis);
