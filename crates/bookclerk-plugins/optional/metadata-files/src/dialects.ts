/**
 * The dialects this plugin serves, by the ids `plugin.toml` declares.
 *
 * A unit test keeps this table and the manifest's
 * `[[metadata_files.dialects]]` rows in step: the same ids, and `render`
 * exactly for the rows with `writes = true`.
 */

import type { MetadataField, MetadataFileRecord } from "@bookclerk/plugin-sdk/workerd";
import { parseAbsJson, renderAbsJson } from "./abs-json.ts";
import { parseLibationJson } from "./libation.ts";
import { parseNfo, renderNfo } from "./nfo.ts";
import { parseDescTxt, parseReaderTxt, renderDescTxt, renderReaderTxt } from "./plain-text.ts";
import { MAX_FILE_BYTES, invalid } from "./text.ts";

/** One file format. */
export interface Dialect {
  /**
   * Reads a file.
   *
   * @param bytes - The whole file.
   * @returns What the file states.
   * @throws {DialectFailure} When the file cannot be read.
   */
  readonly parse: (bytes: Uint8Array) => MetadataFileRecord;
  /**
   * Writes a file, when the format is writable.
   *
   * @param existing - The file as it is now; empty to create it.
   * @param record - Values to write.
   * @param fields - Fields to set.
   * @returns The new file.
   * @throws {DialectFailure} When `existing` cannot be read or the result is
   *   over the size cap.
   */
  readonly render?: (
    existing: Uint8Array,
    record: MetadataFileRecord,
    fields: readonly MetadataField[],
  ) => Uint8Array;
}

/** Every dialect, by id. */
export const DIALECTS: ReadonlyMap<string, Dialect> = new Map<string, Dialect>([
  ["abs_metadata_json", { parse: parseAbsJson, render: renderAbsJson }],
  ["libation_json", { parse: parseLibationJson }],
  ["abs_nfo", { parse: parseNfo, render: renderNfo }],
  ["desc_txt", { parse: parseDescTxt, render: renderDescTxt }],
  ["reader_txt", { parse: parseReaderTxt, render: renderReaderTxt }],
]);

/**
 * Finds a dialect.
 *
 * @param id - Dialect id from the request.
 * @returns The dialect.
 * @throws {DialectFailure} For an id this plugin does not serve.
 */
export function dialect(id: string): Dialect {
  const found = DIALECTS.get(id);
  if (found === undefined) throw invalid(`unknown dialect \`${id}\``);
  return found;
}

/**
 * Parses one file with the dialect the host chose.
 *
 * @param id - Dialect id.
 * @param bytes - The whole file.
 * @returns What the file states.
 * @throws {DialectFailure} For an unknown dialect, a file over the cap or a
 *   file the dialect cannot read.
 */
export function parseFile(id: string, bytes: Uint8Array): MetadataFileRecord {
  const found = dialect(id);
  if (bytes.length > MAX_FILE_BYTES) throw invalid(`file is over the ${MAX_FILE_BYTES} byte cap`);
  return found.parse(bytes);
}

/**
 * Renders one file with the dialect the host chose.
 *
 * @param id - Dialect id.
 * @param existing - The file as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @returns The new file.
 * @throws {DialectFailure} For an unknown or read-only dialect, an existing
 *   file over the cap or unreadable, or a result over the cap.
 */
export function renderFile(
  id: string,
  existing: Uint8Array,
  record: MetadataFileRecord,
  fields: readonly MetadataField[],
): Uint8Array {
  const found = dialect(id);
  if (found.render === undefined) throw invalid(`dialect \`${id}\` is read-only`);
  if (existing.length > MAX_FILE_BYTES) {
    throw invalid(`existing file is over the ${MAX_FILE_BYTES} byte cap`);
  }
  return found.render(existing, record, fields);
}
