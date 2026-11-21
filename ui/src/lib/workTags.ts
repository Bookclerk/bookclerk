import type { WorkTag } from "@/lib/api";

/** Most library tags on one work (the server's bound). */
export const MAX_WORK_TAGS = 32;

/** Longest library tag in UTF-8 bytes (the server's bound). */
export const MAX_WORK_TAG_BYTES = 256;

/**
 * Splits what someone typed into tags: comma-separated, trimmed, blanks
 * and repeats (ignoring case) dropped, first spelling kept.
 *
 * @param text - The typed text, such as `Kids, Bedtime`.
 * @returns The tags in the order typed.
 */
export function parseTagInput(text: string): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const raw of text.split(",")) {
    const tag = raw.trim();
    const key = tag.toLowerCase();
    if (tag && !seen.has(key)) {
      seen.add(key);
      out.push(tag);
    }
  }
  return out;
}

/**
 * Why `tags` can't be saved, or `null` when they can.
 *
 * @param tags - Tags from {@link parseTagInput}.
 * @returns A message for the person.
 */
export function tagInputProblem(tags: readonly string[]): string | null {
  if (tags.length > MAX_WORK_TAGS) return `A book has at most ${MAX_WORK_TAGS} library tags.`;
  const encoder = new TextEncoder();
  if (tags.some((t) => encoder.encode(t).length > MAX_WORK_TAG_BYTES)) {
    return `A library tag is at most ${MAX_WORK_TAG_BYTES} bytes.`;
  }
  return null;
}

/**
 * The tags set in Bookclerk, as the editor's text.
 *
 * @param tags - Every tag of the work.
 * @returns `Kids, Bedtime`.
 */
export function adminTagText(tags: readonly WorkTag[]): string {
  return tags
    .filter((t) => t.source === "admin")
    .map((t) => t.value)
    .join(", ");
}
