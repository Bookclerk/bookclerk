/**
 * Minimal ambient types for the virtual `cloudflare:workers` module.
 * Full Cloudflare Workers types are provided by the workerd / wrangler toolchain
 * at build time; this stub keeps the SDK package self-contained for `tsc`.
 */
declare module "cloudflare:workers" {
  export interface ExecutionContext {
    waitUntil(promise: Promise<unknown>): void;
    passThroughOnException(): void;
  }

  export class RpcTarget {}

  export class WorkerEntrypoint<Env = unknown> {
    constructor(ctx: ExecutionContext, env: Env);
    readonly ctx: ExecutionContext;
    readonly env: Env;
  }
}
