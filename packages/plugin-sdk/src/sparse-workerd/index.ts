/**
 * Sparse out-of-tree workerd launcher (no Rust `bookclerk-workerd` binary).
 *
 * Downloads the pinned Cloudflare `workerd` binary, materializes Cap'n Proto
 * config + bridge assets, and runs describe/health smoke tests against a
 * plugin directory. Import from `@bookclerk/plugin-sdk/sparse-workerd`.
 */

export {
  ensureWorkerd,
  defaultCacheDir,
  loadPin,
  packageRoot,
  binaryName,
  platformKey,
  type WorkerdPin,
  type WorkerdAsset,
} from "./ensure.js";
export {
  materializeConfig,
  pluginGlobalOutbound,
  egressDomainsFor,
  type MaterializeOptions,
  type GeneratedConfig,
} from "./config.js";
export { runSmoke } from "./smoke.js";
