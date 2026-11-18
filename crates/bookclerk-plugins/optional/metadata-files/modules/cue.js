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
 * The writer edits a sheet that exists, line by line: for each field it
 * sets, it removes the lines that field reads from and adds new ones before
 * the first `FILE` (the book's details) or in place of every track (the
 * chapters), and leaves every other line as it was. It cannot create a
 * sheet, because a sheet must name its audio file, and it cannot write
 * chapters into a sheet with several files. CUE has no escape for `"`, so a
 * value's double quotes are written as single quotes.
 */
import { MAX_MS, RecordBuilder, chapterTitle, decodeText, encodeFile, invalid } from "./text.js";
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
function unquote(rest) {
    const trimmed = rest.trim();
    if (!trimmed.startsWith('"'))
        return trimmed;
    const close = trimmed.indexOf('"', 1);
    return close === -1 ? trimmed.slice(1) : trimmed.slice(1, close);
}
/**
 * Splits a sheet into lines with their commands.
 *
 * @param source - The sheet's text.
 * @returns Every line, in order.
 */
function lines(source) {
    const out = [];
    const pattern = /([^\r\n]*)(\r\n|\n|\r|$)/g;
    for (const match of source.matchAll(pattern)) {
        const text = match[1] ?? "";
        const end = match[2] ?? "";
        if (text === "" && end === "")
            break;
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
function indexMs(value) {
    const match = /^01\s+(\d{1,12}):(\d{1,2}):(\d{1,2})\s*$/.exec(value);
    if (match === null)
        return undefined;
    const minutes = Number(match[1]);
    const seconds = Number(match[2]);
    const frames = Number(match[3]);
    if (seconds >= 60 || frames >= FRAMES)
        return undefined;
    const ms = (minutes * 60 + seconds) * 1000 + Math.round((frames * 1000) / FRAMES);
    return ms <= MAX_MS ? ms : undefined;
}
/**
 * Writes milliseconds as a CUE time, rounded to the nearest frame.
 *
 * @param ms - Offset from the start of the file.
 * @returns `mm:ss:ff`.
 */
function cueTime(ms) {
    const frames = Math.round((ms * FRAMES) / 1000);
    const seconds = Math.floor(frames / FRAMES);
    const pad = (n) => String(n).padStart(2, "0");
    return `${pad(Math.floor(seconds / 60))}:${pad(seconds % 60)}:${pad(frames % FRAMES)}`;
}
/**
 * Scans a sheet and checks it names an audio file. The lines before the
 * first track that follows the first `FILE` hold the book's details.
 *
 * @param source - The sheet's text.
 * @returns The scanned sheet.
 * @throws {DialectFailure} When no line is a `FILE` command.
 */
function readSheet(source) {
    const all = lines(source);
    const fileAt = all.findIndex((l) => l.command === "FILE");
    if (fileAt === -1)
        throw invalid("CUE sheet names no audio file");
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
export function parseCue(bytes) {
    const { all, firstTrack, files } = readSheet(decodeText(bytes));
    const header = all.slice(0, firstTrack);
    const out = new RecordBuilder();
    const first = (command) => header.find((l) => l.command === command);
    const every = (command) => header.filter((l) => l.command === command).map((l) => l.value);
    const title = first("TITLE");
    if (title !== undefined)
        out.text("title", title.value);
    const date = first("REM DATE");
    if (date !== undefined)
        out.publishedAt(date.value);
    if (first("REM GENRE") !== undefined)
        out.labels("genres", every("REM GENRE"));
    if (first("PERFORMER") !== undefined)
        out.credits("author", every("PERFORMER"));
    if (files === 1 && firstTrack < all.length) {
        const chapters = [];
        let trackTitle = "";
        let startMs;
        const flush = () => {
            if (startMs !== undefined)
                chapters.push({ title: trackTitle, startMs, depth: 0 });
        };
        for (const l of all.slice(firstTrack)) {
            if (l.command === "TRACK") {
                flush();
                trackTitle = "";
                startMs = undefined;
            }
            else if (l.command === "TITLE") {
                trackTitle = chapterTitle(l.value);
            }
            else if (l.command === "INDEX") {
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
function quote(value) {
    return `"${value.replace(/"/g, "'")}"`;
}
/**
 * Writes the fields asked for into an existing CUE sheet.
 *
 * @param existing - The sheet as it is now.
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @returns The new sheet.
 * @throws {DialectFailure} When `existing` is empty or names no audio file,
 *   when chapters are asked for in a sheet with several files, or when the
 *   result is over the size cap.
 */
export function renderCue(existing, record, fields) {
    const source = decodeText(existing);
    const { all, fileAt, firstTrack, files } = readSheet(source);
    const set = (field) => fields.includes(field);
    const authors = record.creditRoles.includes("author");
    if (set("chapters") && files > 1) {
        throw invalid("cannot write chapters into a CUE sheet with several audio files");
    }
    const nl = /\r\n/.test(source) ? "\r\n" : /\r/.test(source) && !/\n/.test(source) ? "\r" : "\n";
    const drop = (l) => (set("title") && l.command === "TITLE") ||
        (set("publishedAt") && l.command === "REM DATE") ||
        (set("genres") && l.command === "REM GENRE") ||
        (set("credits") && authors && l.command === "PERFORMER");
    const head = [];
    if (set("title") && record.title !== undefined)
        head.push(`TITLE ${quote(record.title)}`);
    if (set("credits") && authors) {
        for (const c of record.credits)
            if (c.role === "author")
                head.push(`PERFORMER ${quote(c.name)}`);
    }
    if (set("publishedAt") && record.publishedAt !== undefined) {
        head.push(`REM DATE ${record.publishedAt}`);
    }
    if (set("genres"))
        for (const genre of record.genres)
            head.push(`REM GENRE ${quote(genre)}`);
    const tracks = [];
    if (set("chapters")) {
        record.chapters.forEach((chapter, i) => {
            tracks.push(`  TRACK ${String(i + 1).padStart(2, "0")} AUDIO`);
            if (chapter.title !== "")
                tracks.push(`    TITLE ${quote(chapter.title)}`);
            tracks.push(`    INDEX 01 ${cueTime(chapter.startMs)}`);
        });
    }
    let out = "";
    all.forEach((l, i) => {
        if (i === fileAt)
            for (const h of head)
                out += h + nl;
        if (i >= firstTrack && set("chapters"))
            return;
        if (i < firstTrack && drop(l))
            return;
        out += l.text + l.end;
    });
    if (tracks.length > 0) {
        if (out !== "" && !/[\r\n]$/.test(out))
            out += nl;
        out += tracks.join(nl) + nl;
    }
    return encodeFile(out);
}
