/**
 * Ports and origins of the local servers the visual checks open, shared by
 * the Playwright config (which starts them) and the specs.
 */

/** Port the static Storybook build is served on. */
export const STORYBOOK_PORT = 6106;

/** Port the website's local server (tools/site/audit/serve.ts) listens on. */
export const SITE_PORT = 6107;

/** Port the collector's page server (tools/diagnostics-collector/visual/serve.ts) listens on. */
export const COLLECTOR_PORT = 6108;

/** Origin of the website's local server. */
export const SITE_ORIGIN = `http://127.0.0.1:${SITE_PORT}`;

/** Origin of the collector's page server. */
export const COLLECTOR_ORIGIN = `http://127.0.0.1:${COLLECTOR_PORT}`;
