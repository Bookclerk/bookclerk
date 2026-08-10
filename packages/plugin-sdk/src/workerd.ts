/// <reference path="./cloudflare-workers.d.ts" />
/**
 * Workerd / Workers RPC guest base class.
 */

export { BookclerkPlugin } from "./bookclerk-plugin.js";
export type {
  BookclerkEnv,
  HandshakeParams,
  HandshakeResult,
  HealthResult,
  DiagnoseResult,
  CliSchema,
  CliInvokeParams,
  CliInvokeResult,
  HostToPluginEvent,
} from "./generated.js";
