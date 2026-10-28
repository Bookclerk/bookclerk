// Bindings the `workers` test project adds (vitest.config.ts), as `env`
// from "cloudflare:test" sees them. The databases themselves (TEST_DB_0 …)
// are read by name in d1.ts.

declare namespace Cloudflare {
  interface Env {
    /** ../migrations, read by vitest.config.ts and applied by workers-setup.ts. */
    TEST_MIGRATIONS: import("cloudflare:test").D1Migration[];
  }
}
