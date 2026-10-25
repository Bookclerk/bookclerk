// Type of generated/docs-shell.json, which `npm run build` writes (see
// DOCS_SHELL_FILE in build.ts) and the Worker entry points import. Checked
// in so the Worker type-checks before a build; the build writes a value of
// exactly this type.

import type { DocsShell } from "../render.ts";

/** The docs page shell the build baked. */
declare const shell: DocsShell;
export = shell;
