// Reads the docs repository's generated.json, which scripts/docs-markdown.py
// writes beside the tree: every generated reference (the plugin SDKs, the
// web API, the Rust crates and the web UI library), where it sits in the
// tree, what it is generated from and which pages it wrote. The Worker
// tells generated pages from hand-written ones by this file rather than by
// their paths, so a reference can move without a change here. The file
// comes from outside the Worker, so it is parsed into a validated manifest
// here, sans-IO, and fuzzed in fuzz.test.ts against an independent oracle
// (fuzz/generated/).

/** The file at the root of each docs version that lists its generated references. */
export const GENERATED_FILE = "generated.json";

/** The `version` of generated.json this parser reads. */
const GENERATED_VERSION = 1;

/**
 * Largest generated.json the Worker reads, in bytes. It lists every
 * generated page once, at under 100 bytes each, so this leaves room for
 * some 20,000 pages.
 */
export const MAX_GENERATED_BYTES = 2 * 1024 * 1024;

/** Most references a manifest may list; the build writes six. */
export const MAX_REFERENCES = 64;

/**
 * A doc key or folder as the manifest may name it: path segments of
 * letters, digits, `.`, `_` and `-`, none empty or starting with `.`.
 */
const DOC_KEY_RE = /^(?:[A-Za-z0-9_-][A-Za-z0-9._-]*\/)*[A-Za-z0-9_-][A-Za-z0-9._-]*$/;

/** Longest key, folder or source path the manifest may hold, in characters. */
export const MAX_GENERATED_PATH_LENGTH = 512;

/** Longest reference title, in characters. */
const MAX_TITLE_LENGTH = 200;

/** The kind of a reference, which says which renderer wrote it. */
export type ReferenceKind = "typescript" | "python" | "rust-sdk" | "web-api" | "rust-crates";

/** The kinds the build writes; a manifest naming another kind is refused. */
const KINDS: readonly ReferenceKind[] = [
  "typescript",
  "python",
  "rust-sdk",
  "web-api",
  "rust-crates",
];

/** One generated reference, as the manifest describes it. */
export interface GeneratedReference {
  /** The folder of the tree it sits in, without a trailing slash (`plugins/sdk/python`). */
  folder: string;
  /** Which renderer wrote it. */
  kind: ReferenceKind;
  /** Its title, as the sidebar and the sitemap name it. */
  title: string;
  /** The repository path it is generated from: a folder or a file. */
  source: string;
  /** Whether `<folder>/README` is a hand-written page the build appended to, rather than one it wrote. */
  landing: boolean;
  /** The keys of every page the build wrote under the folder, in the build's order. */
  pages: readonly string[];
}

/** What generated.json lists. */
export interface GeneratedManifest {
  /** The references, in the build's order. */
  references: readonly GeneratedReference[];
  /**
   * Repository paths of pages or folders generated from something more
   * specific than their reference's source: a crate's folder for its pages
   * under the Rust crates reference.
   */
  sources: Readonly<Record<string, string>>;
}

/**
 * Reads generated.json.
 *
 * The document is `{"version": 1, "references": [...], "sources": {...}}`.
 * Each reference carries `folder`, `kind`, `title`, `source`, `landing`
 * and `pages`; `sources` maps keys or folders to repository paths. A
 * document that is not of that shape, or holds a reference whose folder,
 * kind, title, source or pages are not as described, is refused whole:
 * the Worker then treats every page as hand-written, which only costs
 * generated pages their source line and their sidebar lists.
 *
 * @param json - The file's text.
 * @returns The manifest, or null when the text is not one.
 */
export function parseGenerated(json: string): GeneratedManifest | null {
  let document: unknown;
  try {
    document = JSON.parse(json);
  } catch {
    return null;
  }
  if (!isRecord(document) || document["version"] !== GENERATED_VERSION) return null;
  const list = document["references"];
  if (!Array.isArray(list) || list.length > MAX_REFERENCES) return null;
  const references: GeneratedReference[] = [];
  const folders = new Set<string>();
  for (const entry of list) {
    const reference = readReference(entry);
    if (reference === null || folders.has(reference.folder)) return null;
    folders.add(reference.folder);
    references.push(reference);
  }
  const sources: Record<string, string> = {};
  const rawSources = document["sources"] ?? {};
  if (!isRecord(rawSources)) return null;
  for (const [key, value] of Object.entries(rawSources)) {
    if (!isPath(key) || !isSource(value)) return null;
    sources[key] = value;
  }
  return { references, sources };
}

/**
 * Reads one entry of the manifest's `references`.
 *
 * @param entry - The parsed JSON value.
 * @returns The reference, or null when the entry is not one.
 */
function readReference(entry: unknown): GeneratedReference | null {
  if (!isRecord(entry)) return null;
  const { folder, kind, title, source, landing, pages } = entry;
  if (!isPath(folder) || !isKind(kind) || !isSource(source) || typeof landing !== "boolean")
    return null;
  if (typeof title !== "string" || title.length === 0 || title.length > MAX_TITLE_LENGTH)
    return null;
  if (!Array.isArray(pages)) return null;
  const keys: string[] = [];
  const seen = new Set<string>();
  for (const page of pages) {
    if (!isPath(page) || !page.startsWith(`${folder}/`) || seen.has(page)) return null;
    seen.add(page);
    keys.push(page);
  }
  return { folder, kind, title, source, landing, pages: keys };
}

/**
 * Tells whether a value is a doc key or folder the manifest may name.
 *
 * @param value - The parsed JSON value.
 * @returns True for a plain path of bounded length.
 */
function isPath(value: unknown): value is string {
  return (
    typeof value === "string" && value.length <= MAX_GENERATED_PATH_LENGTH && DOC_KEY_RE.test(value)
  );
}

/**
 * Tells whether a value is a repository path a source line may link to:
 * plain segments like a doc key, with no `..`.
 *
 * @param value - The parsed JSON value.
 * @returns True for a usable source path.
 */
function isSource(value: unknown): value is string {
  return isPath(value) && !value.split("/").includes("..");
}

/**
 * Tells whether a value names a reference kind the build writes.
 *
 * @param value - The parsed JSON value.
 * @returns True for a known kind.
 */
function isKind(value: unknown): value is ReferenceKind {
  return typeof value === "string" && (KINDS as readonly string[]).includes(value);
}

/**
 * Finds the reference a page belongs to: the one whose pages list it, or,
 * for a hand-written landing page, the one whose folder it is the README of.
 *
 * @param manifest - The manifest, or null when the version has none.
 * @param key - The page's key.
 * @returns The reference, or null for a page outside every reference.
 */
export function referenceOf(
  manifest: GeneratedManifest | null,
  key: string,
): GeneratedReference | null {
  if (manifest === null) return null;
  for (const reference of manifest.references) {
    if (reference.pages.includes(key)) return reference;
    if (reference.landing && key === landingKey(reference)) return reference;
  }
  return null;
}

/**
 * The key of a reference's landing page.
 *
 * @param reference - The reference.
 * @returns `<folder>/README`.
 */
export function landingKey(reference: GeneratedReference): string {
  return `${reference.folder}/README`;
}

/**
 * Tells whether a page was written by the build rather than by hand. A
 * hand-written landing page the build appended to is not generated: its
 * source is the file in docs/.
 *
 * @param manifest - The manifest, or null when the version has none.
 * @param key - The page's key.
 * @returns True for a generated page.
 */
export function isGenerated(manifest: GeneratedManifest | null, key: string): boolean {
  return manifest !== null && manifest.references.some((r) => r.pages.includes(key));
}

/**
 * Finds the repository path a generated page comes from: the manifest's
 * entry for the page or for the longest folder holding it, else its
 * reference's source.
 *
 * @param manifest - The manifest.
 * @param key - The page's key.
 * @returns The repository path, or null for a page outside every reference.
 */
export function generatedSource(manifest: GeneratedManifest | null, key: string): string | null {
  if (manifest === null) return null;
  const { sources } = manifest;
  const own = key.endsWith("/README") ? key.slice(0, -"/README".length) : key;
  if (Object.hasOwn(sources, own)) return sources[own] ?? null;
  for (let at = own.lastIndexOf("/"); at > 0; at = own.lastIndexOf("/", at - 1)) {
    const folder = own.slice(0, at);
    if (Object.hasOwn(sources, folder)) return sources[folder] ?? null;
  }
  return referenceOf(manifest, key)?.source ?? null;
}

/** The web API reference's folder and the two pages of it that are not route groups. */
export interface WebApiPages {
  /** Its folder (`server/api`). */
  folder: string;
  /** The index, which lists every route group. */
  index: string;
  /** The schemas page. */
  schemas: string;
}

/**
 * Finds the web API reference, whose pages the site lays out like an
 * OpenAPI viewer.
 *
 * @param manifest - The manifest, or null when the version has none.
 * @returns Its folder and fixed pages, or null when the manifest lists none.
 */
export function webApiPages(manifest: GeneratedManifest | null): WebApiPages | null {
  const reference = manifest?.references.find((r) => r.kind === "web-api");
  return reference === undefined ? null : webApiOf(reference.folder);
}

/**
 * Names the web API's fixed pages from its folder.
 *
 * @param folder - The reference's folder.
 * @returns The folder, its index and its schemas page.
 */
export function webApiOf(folder: string): WebApiPages {
  return { folder, index: `${folder}/README`, schemas: `${folder}/schemas` };
}

/** A module page the sidebar lists beneath a reference's landing page. */
export interface ReferenceModule {
  /** The page's key. */
  key: string;
  /** Its name: the path below the reference's folder, without a trailing README. */
  title: string;
}

/**
 * Lists the pages the sidebar shows beneath a reference's landing page: the
 * pages one level below its folder, in the build's order. Deeper pages
 * (a module's submodules) are reached from their parent.
 *
 * @param reference - The reference.
 * @returns The modules, with the key and name of each.
 */
export function referenceModules(reference: GeneratedReference): ReferenceModule[] {
  const prefix = `${reference.folder}/`;
  const out: ReferenceModule[] = [];
  for (const key of reference.pages) {
    const rest = key.slice(prefix.length);
    const title = rest.endsWith("/README") ? rest.slice(0, -"/README".length) : rest;
    if (title === "README" || title.includes("/")) continue;
    out.push({ key, title });
  }
  return out;
}

/**
 * Tells whether a parsed JSON value is an object (not an array or null).
 *
 * @param value - The value.
 * @returns True for a JSON object.
 */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
