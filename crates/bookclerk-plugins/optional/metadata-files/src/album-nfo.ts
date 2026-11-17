/**
 * Kodi's `album.nfo`: the XML sidecar media centers such as Kodi and
 * Jellyfin read for a music album, which audiobook folders reuse.
 *
 * The reader takes the `<album>` root's children: `title`, `review` as the
 * description, `label` as the publisher, `releasedate` (else `year`) as the
 * date, each `genre`, and each `artist` as an author. Text is plain, not
 * HTML.
 *
 * The writer edits the file in place, as the OPF writer does: for each
 * field it sets, it removes the elements that field reads from and appends
 * new ones at the end of `<album>`. Writing the date also writes `year`;
 * writing authors also drops `albumArtistCredits`, whose artists would
 * otherwise disagree with the new ones.
 */

import type { MetadataField, MetadataFileRecord } from "@bookclerk/plugin-sdk/workerd";
import { RecordBuilder, decodeUtf8, encodeFile, invalid } from "./text.ts";
import {
  type XmlElement,
  appendChildren,
  applyEdits,
  escapeText,
  parseXml,
  removal,
} from "./xml.ts";

/** The file a render creates when there is none yet. */
const SKELETON = `<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<album>
</album>
`;

/**
 * Scans an `album.nfo`.
 *
 * @param source - The file's text.
 * @returns The `<album>` root.
 * @throws {DialectFailure} When the text is not XML or its root is not an
 *   `<album>`.
 */
function readAlbum(source: string): XmlElement {
  const { root } = parseXml(source);
  if (root.name !== "album") throw invalid("album.nfo root element is not an album");
  return root;
}

/**
 * Whether writing a field replaces a child.
 *
 * @param element - Any child of `<album>`.
 * @param field - A field being written.
 * @param authors - Whether the request states authors.
 * @returns True when the child goes.
 */
function replaces(element: XmlElement, field: MetadataField, authors: boolean): boolean {
  switch (field) {
    case "title":
      return element.name === "title";
    case "description":
      return element.name === "review";
    case "publisher":
      return element.name === "label";
    case "publishedAt":
      return element.name === "releasedate" || element.name === "year";
    case "genres":
      return element.name === "genre";
    case "credits":
      return authors && (element.name === "artist" || element.name === "albumArtistCredits");
    case "subtitle":
    case "language":
    case "tags":
    case "abridgement":
    case "expressionKind":
    case "identifiers":
    case "series":
    case "chapters":
    case "duration":
      return false;
  }
}

/**
 * Parses a Kodi `album.nfo`.
 *
 * @param bytes - The whole file.
 * @returns What the file states.
 * @throws {DialectFailure} When the file is not well-formed XML with an
 *   `<album>` root.
 */
export function parseAlbumNfo(bytes: Uint8Array): MetadataFileRecord {
  const kids = readAlbum(decodeUtf8(bytes)).children;
  const first = (name: string): XmlElement | undefined => kids.find((k) => k.name === name);
  const all = (name: string): string[] => kids.filter((k) => k.name === name).map((k) => k.text);
  const out = new RecordBuilder();
  const title = first("title");
  if (title !== undefined) out.text("title", title.text);
  const review = first("review");
  if (review !== undefined) out.description(review.text);
  const label = first("label");
  if (label !== undefined) out.text("publisher", label.text);
  const date = first("releasedate") ?? first("year");
  if (date !== undefined) out.publishedAt(date.text);
  if (first("genre") !== undefined) out.labels("genres", all("genre"));
  if (first("artist") !== undefined) out.credits("author", all("artist"));
  return out.build();
}

/**
 * Writes the fields asked for into a Kodi `album.nfo`.
 *
 * @param existing - The file as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @returns The new file.
 * @throws {DialectFailure} When `existing` cannot be read or the result is
 *   over the size cap.
 */
export function renderAlbumNfo(
  existing: Uint8Array,
  record: MetadataFileRecord,
  fields: readonly MetadataField[],
): Uint8Array {
  const source = existing.length === 0 ? SKELETON : decodeUtf8(existing);
  const album = readAlbum(source);
  const authors = record.creditRoles.includes("author");
  const removed = album.children.filter((kid) =>
    fields.some((field) => replaces(kid, field, authors)),
  );
  const added: string[] = [];
  const set = (field: MetadataField): boolean => fields.includes(field);
  const element = (name: string, text: string): void => {
    added.push(`<${name}>${escapeText(text)}</${name}>`);
  };
  if (set("title") && record.title !== undefined) element("title", record.title);
  if (set("description") && record.description !== undefined) {
    element("review", record.description);
  }
  if (set("publisher") && record.publisher !== undefined) element("label", record.publisher);
  if (set("publishedAt") && record.publishedAt !== undefined) {
    element("releasedate", record.publishedAt);
    element("year", record.publishedAt.slice(0, 4));
  }
  if (set("genres")) for (const genre of record.genres) element("genre", genre);
  if (set("credits") && authors) {
    for (const credit of record.credits)
      if (credit.role === "author") element("artist", credit.name);
  }
  const edits = removed.map((kid) => removal(source, kid));
  edits.push(appendChildren(source, album, added));
  return encodeFile(applyEdits(source, edits));
}
