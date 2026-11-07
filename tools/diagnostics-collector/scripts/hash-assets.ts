// Renames the dashboard's styles and script in public/static/assets/ after
// their contents and rewrites dashboard/asset-urls.ts to match. Run with
// `npm run hash-assets` after editing either file; see asset-names.ts.

import { readdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, URL } from "node:url";

import {
  assetOf,
  assetUrlsModule,
  contentHash,
  hashedName,
  HASHED_ASSETS,
} from "../asset-names.ts";
import type { HashedAsset } from "../asset-names.ts";

/** public/static/assets/ in this checkout. */
const DIR = fileURLToPath(new URL("../public/static/assets/", import.meta.url));

/** dashboard/asset-urls.ts in this checkout. */
const MODULE = fileURLToPath(new URL("../dashboard/asset-urls.ts", import.meta.url));

// Check the whole folder first, so a mistake renames nothing and leaves the
// pages' links as they were.
const current: Partial<Record<HashedAsset, string>> = {};
for (const name of readdirSync(DIR)) {
  const file = assetOf(name);
  if (file === null) throw new Error(`${name} in ${DIR} is not one of ${HASHED_ASSETS.join(", ")}`);
  if (current[file] !== undefined) throw new Error(`${DIR} holds two copies of ${file}`);
  current[file] = name;
}
const missing = HASHED_ASSETS.filter((file) => current[file] === undefined);
if (missing.length > 0) throw new Error(`${DIR} has no ${missing.join(", ")}`);

const names = {} as Record<HashedAsset, string>;
for (const file of HASHED_ASSETS) {
  const name = current[file] ?? "";
  const hashed = hashedName(file, await contentHash(new Uint8Array(readFileSync(join(DIR, name)))));
  if (hashed !== name) {
    renameSync(join(DIR, name), join(DIR, hashed));
    console.log(`${name} -> ${hashed}`);
  }
  names[file] = hashed;
}
writeFileSync(MODULE, assetUrlsModule(names));
