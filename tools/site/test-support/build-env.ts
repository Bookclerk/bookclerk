// The release state worker.test.ts builds the site for; shared with
// site-build.ts, which runs that build before the tests start.

import type { BuildEnv } from "../build.ts";

/** Release state after two releases: v0.2.0 is the default docs version. */
export const ENV: BuildEnv = {
  SITE_COMMIT: "b6adae4d0123456789abcdef0123456789abcdef",
  SITE_BUILT_ON: "2026-10-23",
  SITE_RELEASE_TAG: "v0.2.0",
  SITE_RELEASE_DATE: "2026-12-01",
  SITE_DOCS_VERSIONS: "v0.2.0 v0.1.0",
};
