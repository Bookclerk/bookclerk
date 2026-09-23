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
  assertPathInside,
  ensureDirUnder,
  writeFileUnder,
  copyFileUnder,
  refuseSymlinkPath,
  validateFetchUrl,
  validateSpawnExecutable,
  binaryMatchesPin,
  type WorkerdPin,
  type WorkerdAsset,
} from "./ensure.js";
export {
  materializeConfig,
  allocateWorkerdStateDir,
  pluginGlobalOutbound,
  egressDomainsFor,
  type MaterializeOptions,
  type GeneratedConfig,
} from "./config.js";
export { runSmoke } from "./smoke.js";
