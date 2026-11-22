import type { OutputCopy } from "@/lib/api";

/** A copy an administrator can add to a stored book, with its label. */
export interface CopyProfile {
  /** The copy the convert makes. */
  copy: OutputCopy;
  /** What the menu shows. */
  label: string;
  /** True for a format fewer players open than M4B and MP3. */
  advanced: boolean;
}

/** The copies the convert action offers, common ones first. */
export const COPY_PROFILES: readonly CopyProfile[] = [
  { copy: { format: "m4b", quality: "original" }, label: "M4B", advanced: false },
  { copy: { format: "mp3", quality: "128k" }, label: "MP3, 128 kbit/s", advanced: false },
  { copy: { format: "mp3", quality: "64k" }, label: "MP3, 64 kbit/s", advanced: false },
  { copy: { format: "mp3", quality: "64k-mono" }, label: "MP3, 64 kbit/s mono", advanced: false },
  { copy: { format: "mp3", quality: "v2" }, label: "MP3, VBR level 2", advanced: false },
  {
    copy: { format: "mp3_chapters", quality: "64k-mono" },
    label: "MP3 per chapter, 64 kbit/s mono",
    advanced: false,
  },
  {
    copy: { format: "mp3_chapters", quality: "128k" },
    label: "MP3 per chapter, 128 kbit/s",
    advanced: false,
  },
  { copy: { format: "opus", quality: "64k" }, label: "Opus, 64 kbit/s", advanced: true },
  { copy: { format: "opus", quality: "32k" }, label: "Opus, 32 kbit/s", advanced: true },
  { copy: { format: "opus", quality: "24k-mono" }, label: "Opus, 24 kbit/s mono", advanced: true },
];

/**
 * The key a profile is chosen by in a menu: `format:quality`.
 *
 * @param copy - The copy.
 * @returns Its `format:quality` text.
 */
export function copyKey(copy: OutputCopy): string {
  return `${copy.format}:${copy.quality}`;
}

/**
 * The profiles a menu lists.
 *
 * @param advanced - Whether advanced formats are shown too.
 * @returns The common profiles, then the advanced ones when asked for.
 */
export function visibleProfiles(advanced: boolean): CopyProfile[] {
  return COPY_PROFILES.filter((profile) => advanced || !profile.advanced);
}
