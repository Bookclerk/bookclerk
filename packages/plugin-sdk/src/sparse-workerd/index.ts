/** Sparse out-of-tree workerd launcher (no Rust `bookclerk-workerd` binary). */

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
