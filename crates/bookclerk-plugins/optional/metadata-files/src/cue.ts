/**
 * CUE sheets: the plain-text track lists that rippers and audiobook tools
 * keep next to an audio file, read here as a book's chapters.
 *
 * The reader takes the commands before the first `TRACK` as the book's:
 * `TITLE`, each `PERFORMER` as an author, `REM DATE` and each `REM GENRE`.
 * Each track's `TITLE` and `INDEX 01` (minutes, seconds and 1/75 s frames)
 * become a chapter. A sheet with several `FILE`s times each track from the
 * start of its own file, which the sheet cannot place in the whole book,
 * so its chapters are left unstated. A sheet with no `FILE` is refused.
 *
 * The writer edits a sheet line by line: for each field it sets, it
 * removes the lines that field reads from and adds new ones before the
 * first `FILE`, and leaves every other line as it was. Chapters replace the
 * tracks. When the host passes the book's audio files, the writer can also
 * create a sheet, and writing chapters rebuilds the `FILE` lines from those
 * files: each chapter goes into the file it starts in, timed from that
 * file's start, with track numbers running on across files, and a file
 * that opens partway through a chapter starts with a track that carries
 * that chapter on. With no chapters the files get no tracks. A file keeps the type the sheet gave it, else the type its
 * extension names. Without the audio files it cannot create a sheet or
 * write chapters into a sheet with several files. CUE has no escape for
 * `"`, so a value's double quotes are written as single quotes, and an
 * audio file name with one is refused.
 */

import type {
  MetadataAudioFile,
  MetadataChapter,
  MetadataField,
  MetadataFileRecord,
} from "@bookclerk/plugin-sdk/workerd";
import { MAX_MS, RecordBuilder, chapterTitle, decodeText, encodeFile, invalid } from "./text.ts";

/** One line of a sheet. */
interface Line {
  /** The line's text, without its line break. */
  readonly text: string;
  /** The line break that ends it; empty for a last line with none. */
  readonly end: string;
  /** Its command, upper-cased (`REM DATE` for a `REM` with a known key). */
  readonly command: string;
  /** The rest of the line after the command, unquoted. */
  readonly value: string;
}

/** Frames per second in a CUE time. */
const FRAMES = 75;

/** `REM` keys the dialect reads. */
const REM_KEYS = new Set(["DATE", "GENRE"]);

/**
 * Takes a value from the rest of a line: the text between the first pair
 * of double quotes, else the whole rest.
 *
 * @param rest - Text after the command.
 * @returns The value.
 */
function unquote(rest: string): string {
  const trimmed = rest.trim();
  if (!trimmed.startsWith('"')) return trimmed;
  const close = trimmed.indexOf('"', 1);
  return close === -1 ? trimmed.slice(1) : trimmed.slice(1, close);
}

/**
 * Splits a sheet into lines with their commands.
 *
 * @param source - The sheet's text.
 * @returns Every line, in order.
 */
function lines(source: string): Line[] {
  const out: Line[] = [];
  const pattern = /([^\r\n]*)(\r\n|\n|\r|$)/g;
  for (const match of source.matchAll(pattern)) {
    const text = match[1] ?? "";
    const end = match[2] ?? "";
    if (text === "" && end === "") break;
    const word = /^\s*(\S+)/.exec(text);
    let command = word?.[1]?.toUpperCase() ?? "";
    let rest = text.slice(word?.[0].length ?? 0);
    const key = /^\s+(\S+)/.exec(rest);
    const name = key?.[1]?.toUpperCase() ?? "";
    if (command === "REM" && key !== null && REM_KEYS.has(name)) {
      command = `REM ${name}`;
      rest = rest.slice(key[0].length);
    }
    out.push({ text, end, command, value: unquote(rest) });
  }
  return out;
}

/**
 * Reads a CUE time.
 *
 * @param value - The rest of an `INDEX` line.
 * @returns Milliseconds for `INDEX 01 mm:ss:ff`, else `undefined`.
 */
function indexMs(value: string): number | undefined {
  const match = /^01\s+(\d{1,12}):(\d{1,2}):(\d{1,2})\s*$/.exec(value);
  if (match === null) return undefined;
  const minutes = Number(match[1]);
  const seconds = Number(match[2]);
  const frames = Number(match[3]);
  if (seconds >= 60 || frames >= FRAMES) return undefined;
  const ms = (minutes * 60 + seconds) * 1000 + Math.round((frames * 1000) / FRAMES);
  return ms <= MAX_MS ? ms : undefined;
}

/**
 * Writes milliseconds as a CUE time, rounded to the nearest frame.
 *
 * @param ms - Offset from the start of the file.
 * @returns `mm:ss:ff`.
 */
function cueTime(ms: number): string {
  const frames = Math.round((ms * FRAMES) / 1000);
  const seconds = Math.floor(frames / FRAMES);
  const pad = (n: number): string => String(n).padStart(2, "0");
  return `${pad(Math.floor(seconds / 60))}:${pad(seconds % 60)}:${pad(frames % FRAMES)}`;
}

/** A scanned sheet. */
interface Sheet {
  /** Every line. */
  readonly all: Line[];
  /** Index of the first `FILE` line. */
  readonly fileAt: number;
  /** Index of the first `TRACK` after it, or the line count. */
  readonly firstTrack: number;
  /** How many `FILE` lines the sheet has. */
  readonly files: number;
}

/**
 * Scans a sheet and checks it names an audio file. The lines before the
 * first track that follows the first `FILE` hold the book's details.
 *
 * @param source - The sheet's text.
 * @returns The scanned sheet.
 * @throws {DialectFailure} When no line is a `FILE` command.
 */
function readSheet(source: string): Sheet {
  const all = lines(source);
  const fileAt = all.findIndex((l) => l.command === "FILE");
  if (fileAt === -1) throw invalid("CUE sheet names no audio file");
  const files = all.filter((l) => l.command === "FILE").length;
  const track = all.findIndex((l, i) => i > fileAt && l.command === "TRACK");
  return { all, fileAt, firstTrack: track === -1 ? all.length : track, files };
}

/**
 * Parses a CUE sheet.
 *
 * @param bytes - The whole file.
 * @returns What the sheet states.
 * @throws {DialectFailure} When the sheet names no audio file.
 */
export function parseCue(bytes: Uint8Array): MetadataFileRecord {
  const { all, firstTrack, files } = readSheet(decodeText(bytes));
  const header = all.slice(0, firstTrack);
  const out = new RecordBuilder();
  const first = (command: string): Line | undefined => header.find((l) => l.command === command);
  const every = (command: string): string[] =>
    header.filter((l) => l.command === command).map((l) => l.value);
  const title = first("TITLE");
  if (title !== undefined) out.text("title", title.value);
  const date = first("REM DATE");
  if (date !== undefined) out.publishedAt(date.value);
  if (first("REM GENRE") !== undefined) out.labels("genres", every("REM GENRE"));
  if (first("PERFORMER") !== undefined) out.credits("author", every("PERFORMER"));
  if (files === 1 && firstTrack < all.length) {
    const chapters: MetadataChapter[] = [];
    let trackTitle = "";
    let startMs: number | undefined;
    const flush = (): void => {
      if (startMs !== undefined) chapters.push({ title: trackTitle, startMs, depth: 0 });
    };
    for (const l of all.slice(firstTrack)) {
      if (l.command === "TRACK") {
        flush();
        trackTitle = "";
        startMs = undefined;
      } else if (l.command === "TITLE") {
        trackTitle = chapterTitle(l.value);
      } else if (l.command === "INDEX") {
        startMs ??= indexMs(l.value);
      }
    }
    flush();
    out.chapters(chapters);
  }
  return out.build();
}

/**
 * Quotes a value for a CUE line.
 *
 * @param value - Single-line text.
 * @returns The value in double quotes, its own double quotes made single.
 */
function quote(value: string): string {
  return `"${value.replace(/"/g, "'")}"`;
}

/**
 * Reads a `FILE` line's name and type.
 *
 * @param line - A `FILE` line.
 * @returns The file name and the type after it (empty when missing).
 */
function fileLine(line: Line): { name: string; type: string } {
  const rest = line.text.replace(/^\s*\S+\s*/, "");
  if (rest.startsWith('"')) {
    const close = rest.indexOf('"', 1);
    if (close === -1) return { name: rest.slice(1), type: "" };
    return { name: rest.slice(1, close), type: rest.slice(close + 1).trim() };
  }
  const words = rest.trim().split(/\s+/);
  if (words.length < 2) return { name: words[0] ?? "", type: "" };
  return { name: words.slice(0, -1).join(" "), type: words[words.length - 1] ?? "" };
}

/**
 * Picks a `FILE` type from a file name's extension.
 *
 * @param name - The audio file's name.
 * @returns `MP3`, `AIFF`, `MP4` or, for anything else, `WAVE`.
 */
function fileType(name: string): string {
  const ext = /\.([^.]+)$/.exec(name)?.[1]?.toLowerCase() ?? "";
  if (ext === "mp3") return "MP3";
  if (ext === "aif" || ext === "aiff") return "AIFF";
  if (["m4a", "m4b", "mp4", "aac"].includes(ext)) return "MP4";
  return "WAVE";
}

/**
 * Lays chapters out across audio files as `FILE` and `TRACK` lines.
 *
 * @param audio - The book's audio files in playback order.
 * @param chapters - Chapters timed from the start of the first file.
 * @param types - Types the sheet already gives files, by name.
 * @returns The lines, without line breaks.
 * @throws {DialectFailure} When a file name has a double quote, or when
 *   chapters must be placed past a file of unknown length.
 */
function fileBlocks(
  audio: readonly MetadataAudioFile[],
  chapters: readonly MetadataChapter[],
  types: ReadonlyMap<string, string>,
): string[] {
  if (audio.some((f) => f.name.includes('"'))) {
    throw invalid("cannot write an audio file name with a double quote into a CUE sheet");
  }
  if (chapters.length > 0 && audio.slice(0, -1).some((f) => f.durationMs === 0)) {
    throw invalid("cannot write chapters across audio files of unknown length");
  }
  const out: string[] = [];
  let track = 0;
  let playing: string | undefined;
  let next = 0;
  let fileStart = 0;
  audio.forEach((file, i) => {
    out.push(`FILE ${quote(file.name)} ${types.get(file.name) ?? fileType(file.name)}`);
    const last = i === audio.length - 1;
    const fileEnd = fileStart + file.durationMs;
    const add = (title: string, ms: number): void => {
      track += 1;
      out.push(`  TRACK ${String(track).padStart(2, "0")} AUDIO`);
      if (title !== "") out.push(`    TITLE ${quote(title)}`);
      out.push(`    INDEX 01 ${cueTime(ms)}`);
    };
    let opened = false;
    while (next < chapters.length) {
      const chapter = chapters[next];
      if (chapter === undefined || (!last && chapter.startMs >= fileEnd)) break;
      if (!opened && chapter.startMs > fileStart && playing !== undefined) add(playing, 0);
      add(chapter.title, Math.max(0, chapter.startMs - fileStart));
      opened = true;
      playing = chapter.title;
      next += 1;
    }
    if (!opened && playing !== undefined) add(playing, 0);
    fileStart = fileEnd;
  });
  return out;
}

/**
 * Writes the fields asked for into a CUE sheet.
 *
 * @param existing - The sheet as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @param audio - The book's audio files in playback order; empty when the
 *   host does not know them.
 * @returns The new sheet.
 * @throws {DialectFailure} When `existing` names no audio file and no
 *   audio files are given, when chapters are asked for in a sheet with
 *   several files and no audio files are given, when the audio files cannot
 *   be written, or when the result is over the size cap.
 */
export function renderCue(
  existing: Uint8Array,
  record: MetadataFileRecord,
  fields: readonly MetadataField[],
  audio: readonly MetadataAudioFile[] = [],
): Uint8Array {
  const set = (field: MetadataField): boolean => fields.includes(field);
  const source = decodeText(existing);
  const creating = existing.length === 0 && audio.length > 0;
  const { all, fileAt, firstTrack, files } = creating
    ? { all: [], fileAt: 0, firstTrack: 0, files: 0 }
    : readSheet(source);
  const rebuild = audio.length > 0 && (creating || set("chapters"));
  if (!rebuild && set("chapters") && files > 1) {
    throw invalid("cannot write chapters into a CUE sheet with several audio files");
  }
  const authors = record.creditRoles.includes("author");
  const nl = /\r\n/.test(source) ? "\r\n" : /\r/.test(source) && !/\n/.test(source) ? "\r" : "\n";
  const drop = (l: Line): boolean =>
    (set("title") && l.command === "TITLE") ||
    (set("publishedAt") && l.command === "REM DATE") ||
    (set("genres") && l.command === "REM GENRE") ||
    (set("credits") && authors && l.command === "PERFORMER");
  const head: string[] = [];
  if (set("title") && record.title !== undefined) head.push(`TITLE ${quote(record.title)}`);
  if (set("credits") && authors) {
    for (const c of record.credits)
      if (c.role === "author") head.push(`PERFORMER ${quote(c.name)}`);
  }
  if (set("publishedAt") && record.publishedAt !== undefined) {
    head.push(`REM DATE ${record.publishedAt}`);
  }
  if (set("genres")) for (const genre of record.genres) head.push(`REM GENRE ${quote(genre)}`);
  let tail: string[] = [];
  if (rebuild) {
    const types = new Map<string, string>();
    for (const l of all) {
      if (l.command !== "FILE") continue;
      const { name, type } = fileLine(l);
      if (type !== "" && !types.has(name)) types.set(name, type);
    }
    tail = fileBlocks(audio, set("chapters") ? record.chapters : [], types);
  } else if (set("chapters")) {
    record.chapters.forEach((chapter, i) => {
      tail.push(`  TRACK ${String(i + 1).padStart(2, "0")} AUDIO`);
      if (chapter.title !== "") tail.push(`    TITLE ${quote(chapter.title)}`);
      tail.push(`    INDEX 01 ${cueTime(chapter.startMs)}`);
    });
  }
  // A chapter write keeps the book's lines, before the first track; a
  // rebuild also drops their `FILE` lines, which it writes anew.
  const keep = set("chapters") || rebuild ? firstTrack : all.length;
  let out = "";
  all.forEach((l, i) => {
    if (i === fileAt) for (const h of head) out += h + nl;
    if (i >= keep || (rebuild && l.command === "FILE")) return;
    if (i < firstTrack && drop(l)) return;
    out += l.text + l.end;
  });
  if (creating) for (const h of head) out += h + nl;
  if (tail.length > 0) {
    if (out !== "" && !/[\r\n]$/.test(out)) out += nl;
    out += tail.join(nl) + nl;
  }
  return encodeFile(out);
}
