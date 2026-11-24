/**
 * Backblaze B2 native API: what the destination reads from B2, sans-IO.
 *
 * Every reply B2 sends (sign-in, bucket lists, upload URLs, file versions,
 * listings, uploaded parts, errors and download headers) comes from outside
 * the library, so each reader here checks it into a typed value and fails
 * with {@link B2ReplyError} instead of passing on what it cannot vouch for.
 * Nothing here does I/O: the backend hands in bytes and headers, and the
 * fuzz test drives the same functions. File names are percent-encoded the
 * way B2 documents for headers and download URLs.
 */
/** Largest B2 reply read, in bytes; a longer reply is refused. */
export const MAX_B2_REPLY_BYTES = 16 * 1024 * 1024;
/** Most files one listing reply may hold (B2's own cap is 10 000). */
export const MAX_LIST_FILES = 10_000;
/** Longest B2 file name, in UTF-8 bytes (B2's own limit). */
export const MAX_FILE_NAME_BYTES = 1024;
/** Longest id (account, bucket or file id) accepted, in bytes. */
export const MAX_ID_BYTES = 256;
/** Longest authorization token accepted, in bytes. */
export const MAX_TOKEN_BYTES = 4096;
/** Longest API, download or upload URL accepted, in bytes. */
export const MAX_URL_BYTES = 2048;
/** Most custom file-info entries one file may carry. */
export const MAX_FILE_INFO_ENTRIES = 64;
/** Longest file-info key or value accepted, in bytes. */
export const MAX_FILE_INFO_BYTES = 8192;
/** Longest error code or message kept, in bytes. */
export const MAX_ERROR_TEXT_BYTES = 1024;
/** Most parts one large file may have (B2's own limit). */
export const MAX_PARTS = 10_000;
/** Most headers read from one download reply. */
export const MAX_DOWNLOAD_HEADERS = 128;
/** A B2 reply this destination cannot use. */
export class B2ReplyError extends Error {
    /** `too_large` for an oversized reply, `malformed` for anything else. */
    kind;
    /**
     * Creates a reply error.
     *
     * @param kind - Why the reply was refused.
     * @param message - What was wrong.
     */
    constructor(kind, message) {
        super(message);
        this.name = "B2ReplyError";
        this.kind = kind;
    }
}
/**
 * A malformed-reply error.
 *
 * @param what - What was wrong.
 * @returns The error.
 */
function malformed(what) {
    return new B2ReplyError("malformed", `B2 reply is malformed: ${what}`);
}
/** Shared encoder for byte lengths. */
const ENCODER = new TextEncoder();
/**
 * The UTF-8 length of `text`, in bytes.
 *
 * @param text - Any string.
 * @returns Its UTF-8 byte length.
 */
export function utf8Length(text) {
    return ENCODER.encode(text).length;
}
/**
 * True when every byte of `text` is a visible ASCII character (0x21-0x7e).
 *
 * @param text - Any string.
 * @returns Whether it is header-safe visible ASCII.
 */
function isAsciiGraphic(text) {
    for (let i = 0; i < text.length; i++) {
        const code = text.charCodeAt(i);
        if (code < 0x21 || code > 0x7e)
            return false;
    }
    return true;
}
/**
 * True when `text` holds a Unicode control character (C0, DEL or C1), the
 * set Rust's `char::is_control` names.
 *
 * @param text - Any string.
 * @returns Whether a control character is present.
 */
export function hasControl(text) {
    for (let i = 0; i < text.length; i++) {
        const code = text.charCodeAt(i);
        if (code < 0x20 || (code >= 0x7f && code <= 0x9f))
            return true;
    }
    return false;
}
/**
 * Checks `name` against B2's file-name rules plus the destination's own: not
 * empty, at most {@link MAX_FILE_NAME_BYTES} bytes, no control character or
 * DEL, no backslash, and no leading, trailing or doubled `/`.
 *
 * @param name - A full B2 file name (prefix included).
 * @returns `null` when the name is acceptable, else why it is not.
 */
export function fileNameProblem(name) {
    if (name.length === 0)
        return "B2 file name is empty";
    if (utf8Length(name) > MAX_FILE_NAME_BYTES) {
        return `B2 file name is over ${MAX_FILE_NAME_BYTES} bytes`;
    }
    for (let i = 0; i < name.length; i++) {
        const code = name.charCodeAt(i);
        if (code < 0x20 || code === 0x7f)
            return "B2 file name holds a control character";
    }
    if (name.includes("\\"))
        return "B2 file name holds a backslash";
    if (name.startsWith("/") || name.endsWith("/") || name.includes("//")) {
        return "B2 file name starts or ends with '/' or holds '//'";
    }
    return null;
}
/**
 * True when B2 keeps `byte` as is in an encoded name.
 *
 * @param byte - One UTF-8 byte.
 * @returns Whether it needs no escape.
 */
function keeps(byte) {
    return ((byte >= 0x30 && byte <= 0x39) ||
        (byte >= 0x41 && byte <= 0x5a) ||
        (byte >= 0x61 && byte <= 0x7a) ||
        byte === 0x2d ||
        byte === 0x2e ||
        byte === 0x5f ||
        byte === 0x7e ||
        byte === 0x2f);
}
/** Upper-case hex digits. */
const HEX = "0123456789ABCDEF";
/**
 * Percent-encodes a file name or file-info value the way B2 asks for headers
 * and download URLs: every UTF-8 byte except letters, digits, `-._~/` as
 * `%XX` with upper-case hex.
 *
 * @param name - The text to encode.
 * @returns The encoded text.
 */
export function encodeFileName(name) {
    let out = "";
    for (const byte of ENCODER.encode(name)) {
        if (keeps(byte)) {
            out += String.fromCharCode(byte);
        }
        else {
            out += `%${HEX[byte >> 4]}${HEX[byte & 0x0f]}`;
        }
    }
    return out;
}
/**
 * The value of one hex digit.
 *
 * @param code - A character code.
 * @returns The digit's value, or `null` when it is not hex.
 */
function hexValue(code) {
    if (code >= 0x30 && code <= 0x39)
        return code - 0x30;
    if (code >= 0x61 && code <= 0x66)
        return code - 0x61 + 10;
    if (code >= 0x41 && code <= 0x46)
        return code - 0x41 + 10;
    return null;
}
/** Strict UTF-8 decoder. */
const STRICT_UTF8 = new TextDecoder("utf-8", { fatal: true });
/**
 * Decodes a percent-encoded B2 header value (`+` reads as a space).
 *
 * @param text - The header value.
 * @returns The decoded text, or `null` for a bad escape or invalid UTF-8.
 */
export function decodeFileName(text) {
    const bytes = [];
    for (let i = 0; i < text.length;) {
        const code = text.charCodeAt(i);
        if (code === 0x25) {
            const high = i + 1 < text.length ? hexValue(text.charCodeAt(i + 1)) : null;
            const low = i + 2 < text.length ? hexValue(text.charCodeAt(i + 2)) : null;
            if (high === null || low === null)
                return null;
            bytes.push((high << 4) | low);
            i += 3;
        }
        else if (code === 0x2b) {
            bytes.push(0x20);
            i += 1;
        }
        else {
            // Header values are Latin-1 text; anything past ASCII is re-encoded.
            const point = text.codePointAt(i) ?? 0;
            for (const byte of ENCODER.encode(String.fromCodePoint(point)))
                bytes.push(byte);
            i += point > 0xffff ? 2 : 1;
        }
    }
    try {
        return STRICT_UTF8.decode(new Uint8Array(bytes));
    }
    catch {
        return null;
    }
}
/**
 * Checks a bearer token B2 handed out: 1 to {@link MAX_TOKEN_BYTES} visible
 * ASCII bytes, so it can go in a header as is.
 *
 * @param token - The token.
 * @returns The token.
 * @throws {B2ReplyError} When it is not header-safe.
 */
export function parseAuthToken(token) {
    if (typeof token !== "string" ||
        token.length === 0 ||
        token.length > MAX_TOKEN_BYTES ||
        !isAsciiGraphic(token)) {
        throw malformed("authorization token is not a header-safe token");
    }
    return token;
}
/**
 * Checks a URL B2 handed out: plain `https://`, a host of letters, digits,
 * `.`, `-` and `:` with no credentials, query or fragment. A trailing `/` is
 * dropped. `allowLoopbackHttp` lets tests point the backend at a local fake.
 *
 * @param url - The URL.
 * @param allowLoopbackHttp - Also accept `http://127.0.0.1:<port>`.
 * @returns The URL without a trailing `/`.
 * @throws {B2ReplyError} When it is not a plain https URL.
 */
export function parseB2Url(url, allowLoopbackHttp = false) {
    const bad = () => malformed("B2 URL is not a plain https URL");
    if (typeof url !== "string" || url.length > MAX_URL_BYTES || !isAsciiGraphic(url))
        throw bad();
    let rest;
    if (url.startsWith("https://")) {
        rest = url.slice("https://".length);
    }
    else if (allowLoopbackHttp && url.startsWith("http://127.0.0.1:")) {
        rest = url.slice("http://".length);
    }
    else {
        throw bad();
    }
    const host = rest.split("/")[0] ?? "";
    if (host.length === 0 ||
        host.includes("@") ||
        !/^[A-Za-z0-9.:-]+$/.test(host) ||
        rest.includes("?") ||
        rest.includes("#")) {
        throw bad();
    }
    return url.replace(/\/+$/, "");
}
/**
 * Checks an id: 1 to {@link MAX_ID_BYTES} visible ASCII bytes.
 *
 * @param what - What the id names, for the error.
 * @param id - The id.
 * @returns The id.
 * @throws {B2ReplyError} When it is not a B2 id.
 */
function checkId(what, id) {
    if (typeof id !== "string" ||
        id.length === 0 ||
        id.length > MAX_ID_BYTES ||
        !isAsciiGraphic(id)) {
        throw malformed(`${what} is not a B2 id`);
    }
    return id;
}
/**
 * Checks a bucket name against B2's rule: 1 to 63 letters, digits, `-` and
 * `.`, not starting or ending with `.`, no `..`, and not an IPv4 address.
 *
 * @param name - The bucket name.
 * @returns The name.
 * @throws {B2ReplyError} When it breaks the rule.
 */
function checkBucketName(name) {
    if (typeof name !== "string")
        throw malformed("bucket name breaks B2's naming rule");
    const parts = name.split(".");
    const ipv4 = parts.length === 4 && parts.every((part) => /^[0-9]+$/.test(part));
    if (name.length === 0 ||
        name.length > 63 ||
        !/^[A-Za-z0-9.-]+$/.test(name) ||
        name.startsWith(".") ||
        name.endsWith(".") ||
        name.includes("..") ||
        ipv4) {
        throw malformed("bucket name breaks B2's naming rule");
    }
    return name;
}
/**
 * True when `value` is a JSON object (not an array or `null`).
 *
 * @param value - Any parsed JSON value.
 * @returns Whether it is a plain object.
 */
export function isObject(value) {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}
/**
 * Parses a reply body as one JSON object.
 *
 * @param what - What the reply is, for errors.
 * @param bytes - The body.
 * @returns The object.
 * @throws {B2ReplyError} When the body is too large, not UTF-8, not JSON or
 *   not an object.
 */
export function decodeObject(what, bytes) {
    if (bytes.length > MAX_B2_REPLY_BYTES) {
        throw new B2ReplyError("too_large", `B2 reply is over ${MAX_B2_REPLY_BYTES} bytes`);
    }
    let text;
    try {
        text = STRICT_UTF8.decode(bytes);
    }
    catch {
        throw malformed(`${what}: not UTF-8`);
    }
    let value;
    try {
        value = JSON.parse(text);
    }
    catch (err) {
        throw malformed(`${what}: ${err.message}`);
    }
    if (!isObject(value))
        throw malformed(`${what}: expected a JSON object`);
    return value;
}
/**
 * Reads a required string field.
 *
 * @param object - The JSON object.
 * @param field - The field name.
 * @param what - What the object is, for errors.
 * @returns The string.
 * @throws {B2ReplyError} When the field is missing or not a string.
 */
function text(object, field, what) {
    const value = object[field];
    if (typeof value !== "string")
        throw malformed(`${what}: missing field \`${field}\``);
    return value;
}
/**
 * Reads an optional string field (`null` and absent read as `null`).
 *
 * @param object - The JSON object.
 * @param field - The field name.
 * @param what - What the object is, for errors.
 * @returns The string, or `null`.
 * @throws {B2ReplyError} When the field holds something other than a string.
 */
function optionalText(object, field, what) {
    const value = object[field];
    if (value === undefined || value === null)
        return null;
    if (typeof value !== "string")
        throw malformed(`${what}: \`${field}\` is not a string`);
    return value;
}
/**
 * True when `value` is a non-negative safe integer (a JSON u64 this code can
 * hold exactly).
 *
 * @param value - Any parsed JSON value.
 * @returns Whether it is a usable count.
 */
export function isCount(value) {
    return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
/**
 * Reads a required whole-number field.
 *
 * @param object - The JSON object.
 * @param field - The field name.
 * @param what - What the object is, for errors.
 * @returns The number.
 * @throws {B2ReplyError} When it is missing or not a non-negative integer.
 */
function count(object, field, what) {
    const value = object[field];
    if (!isCount(value))
        throw malformed(`${what}: \`${field}\` is not a whole number`);
    return value;
}
/**
 * Reads a required object field.
 *
 * @param object - The JSON object.
 * @param field - The field name.
 * @param what - What the object is, for errors.
 * @returns The nested object.
 * @throws {B2ReplyError} When it is missing or not an object.
 */
function nested(object, field, what) {
    const value = object[field];
    if (!isObject(value))
        throw malformed(`${what}: \`${field}\` is not a JSON object`);
    return value;
}
/**
 * Reads a required array of objects.
 *
 * @param object - The JSON object.
 * @param field - The field name.
 * @param what - What the object is, for errors.
 * @returns The objects.
 * @throws {B2ReplyError} When it is missing, not an array, or holds a
 *   non-object.
 */
function objects(object, field, what) {
    const value = object[field];
    if (!Array.isArray(value))
        throw malformed(`${what}: \`${field}\` is not a list`);
    return value.map((entry) => {
        if (!isObject(entry))
            throw malformed(`${what}: \`${field}\` holds a non-object`);
        return entry;
    });
}
/**
 * Reads the reply to `b2_authorize_account` (API v3).
 *
 * @param bytes - The reply body.
 * @param allowLoopbackHttp - Accept loopback http URLs (tests only).
 * @returns The checked authorization.
 * @throws {B2ReplyError} When the reply is unusable.
 */
export function parseAuthorization(bytes, allowLoopbackHttp = false) {
    const what = "authorization reply";
    const wire = decodeObject(what, bytes);
    const accountId = checkId("account id", text(wire, "accountId", what));
    const tokenText = text(wire, "authorizationToken", what);
    const storage = nested(nested(wire, "apiInfo", what), "storageApi", what);
    const apiUrl = text(storage, "apiUrl", what);
    const downloadUrl = text(storage, "downloadUrl", what);
    const recommended = count(storage, "recommendedPartSize", what);
    const minimum = count(storage, "absoluteMinimumPartSize", what);
    if (minimum === 0 || recommended < minimum) {
        throw malformed("authorization reply has impossible part sizes");
    }
    const bucketId = optionalText(storage, "bucketId", what);
    let bucket = null;
    if (bucketId !== null) {
        const id = checkId("bucket id", bucketId);
        const name = optionalText(storage, "bucketName", what);
        if (name === null)
            throw malformed("the key's bucket no longer exists");
        bucket = { id, name: checkBucketName(name) };
    }
    const prefixText = optionalText(storage, "namePrefix", what);
    const namePrefix = prefixText === null || prefixText === "" ? null : prefixText;
    if (namePrefix !== null &&
        (utf8Length(namePrefix) > MAX_FILE_NAME_BYTES || hasControl(namePrefix))) {
        throw malformed("the key's name prefix is not a file-name prefix");
    }
    const rawCapabilities = storage["capabilities"];
    let capabilities = [];
    if (rawCapabilities !== undefined && rawCapabilities !== null) {
        if (!Array.isArray(rawCapabilities) || !rawCapabilities.every((c) => typeof c === "string")) {
            throw malformed(`${what}: \`capabilities\` is not a list of names`);
        }
        capabilities = rawCapabilities;
    }
    if (capabilities.length > 64 ||
        capabilities.some((c) => c.length === 0 || c.length > 64 || !/^[A-Za-z0-9]+$/.test(c))) {
        throw malformed("the key's capabilities are not capability names");
    }
    return {
        accountId,
        token: parseAuthToken(tokenText),
        apiUrl: parseB2Url(apiUrl, allowLoopbackHttp),
        downloadUrl: parseB2Url(downloadUrl, allowLoopbackHttp),
        recommendedPartSize: recommended,
        absoluteMinimumPartSize: minimum,
        bucket,
        namePrefix,
        capabilities,
    };
}
/**
 * Reads the reply to `b2_list_buckets`.
 *
 * @param bytes - The reply body.
 * @returns The buckets, checked.
 * @throws {B2ReplyError} When the reply is unusable.
 */
export function parseBuckets(bytes) {
    const what = "bucket list";
    const wire = objects(decodeObject(what, bytes), "buckets", what);
    if (wire.length > 1000)
        throw malformed("bucket list holds more than 1000 buckets");
    return wire.map((bucket) => ({
        id: checkId("bucket id", text(bucket, "bucketId", what)),
        name: checkBucketName(text(bucket, "bucketName", what)),
    }));
}
/**
 * Reads the reply to `b2_get_upload_url` or `b2_get_upload_part_url`.
 *
 * @param bytes - The reply body.
 * @param allowLoopbackHttp - Accept loopback http URLs (tests only).
 * @returns The target, checked.
 * @throws {B2ReplyError} When the reply is unusable.
 */
export function parseUploadTarget(bytes, allowLoopbackHttp = false) {
    const what = "upload URL reply";
    const wire = decodeObject(what, bytes);
    return {
        url: parseB2Url(text(wire, "uploadUrl", what), allowLoopbackHttp),
        token: parseAuthToken(text(wire, "authorizationToken", what)),
    };
}
/**
 * True when a version with `action` is a stored object (an upload or copy).
 *
 * @param action - The version's action.
 * @returns Whether it holds bytes.
 */
export function isStored(action) {
    switch (action) {
        case "upload":
        case "copy":
            return true;
        case "hide":
        case "start":
        case "folder":
            return false;
    }
}
/**
 * Reads a file action.
 *
 * @param action - The wire text.
 * @returns The action.
 * @throws {B2ReplyError} When B2 does not document it.
 */
function fileAction(action) {
    switch (action) {
        case "upload":
        case "copy":
        case "hide":
        case "start":
        case "folder":
            return action;
        default:
            throw malformed("file action is not one B2 documents");
    }
}
/**
 * Reads a SHA-1 B2 states, dropping its `unverified:` marker.
 *
 * @param value - The wire text, when present.
 * @returns Lower-case hex, or `null` when absent or not a SHA-1.
 */
function sha1Of(value) {
    if (typeof value !== "string")
        return null;
    const hex = value.startsWith("unverified:") ? value.slice("unverified:".length) : value;
    return /^[0-9A-Fa-f]{40}$/.test(hex) ? hex.toLowerCase() : null;
}
/**
 * Checks one wire file entry into a {@link FileVersion}.
 *
 * @param wire - The JSON object.
 * @returns The version.
 * @throws {B2ReplyError} When the entry is unusable.
 */
function fileVersion(wire) {
    const what = "file version";
    const action = fileAction(text(wire, "action", what));
    const fileName = text(wire, "fileName", what);
    const idText = optionalText(wire, "fileId", what);
    let fileId;
    if (action === "folder") {
        fileId = null;
    }
    else if (idText !== null) {
        fileId = checkId("file id", idText);
    }
    else {
        throw malformed("file version has no file id");
    }
    const checked = action === "folder" && fileName.endsWith("/") ? fileName.slice(0, -1) : fileName;
    const problem = fileNameProblem(checked);
    if (problem !== null)
        throw malformed(problem);
    const rawInfo = wire["fileInfo"];
    const fileInfo = new Map();
    if (rawInfo !== undefined && rawInfo !== null) {
        if (!isObject(rawInfo))
            throw malformed(`${what}: \`fileInfo\` is not a JSON object`);
        for (const [key, value] of Object.entries(rawInfo)) {
            if (typeof value !== "string")
                throw malformed(`${what}: file info values are strings`);
            fileInfo.set(key, value);
        }
    }
    if (fileInfo.size > MAX_FILE_INFO_ENTRIES ||
        [...fileInfo].some(([key, value]) => utf8Length(key) > MAX_FILE_INFO_BYTES || utf8Length(value) > MAX_FILE_INFO_BYTES)) {
        throw malformed("file info is over its limits");
    }
    const typeText = optionalText(wire, "contentType", what);
    if (typeText !== null && (utf8Length(typeText) > 256 || hasControl(typeText))) {
        throw malformed("content type is not a media type");
    }
    const contentType = typeText === null || typeText === "" ? null : typeText;
    const rawLength = wire["contentLength"];
    let contentLength;
    if (rawLength === undefined || rawLength === null) {
        if (isStored(action))
            throw malformed("stored file version has no length");
        contentLength = 0;
    }
    else if (isCount(rawLength)) {
        contentLength = rawLength;
    }
    else {
        throw malformed(`${what}: \`contentLength\` is not a whole number`);
    }
    const rawStamp = wire["uploadTimestamp"];
    let uploadTimestampMs = 0;
    if (rawStamp !== undefined && rawStamp !== null) {
        if (!isCount(rawStamp))
            throw malformed(`${what}: \`uploadTimestamp\` is not a whole number`);
        uploadTimestampMs = rawStamp;
    }
    const contentSha1 = sha1Of(optionalText(wire, "contentSha1", what)) ?? sha1Of(fileInfo.get("large_file_sha1"));
    return {
        fileId,
        fileName,
        contentLength,
        contentSha1,
        contentType,
        uploadTimestampMs,
        action,
        fileInfo,
    };
}
/**
 * Reads a reply that is one file version (upload, copy, start, finish, file
 * info).
 *
 * @param bytes - The reply body.
 * @returns The version.
 * @throws {B2ReplyError} When the reply is unusable.
 */
export function parseFileVersion(bytes) {
    return fileVersion(decodeObject("file reply", bytes));
}
/**
 * Checks a listing's files.
 *
 * @param files - The wire entries.
 * @returns The versions.
 * @throws {B2ReplyError} When there are too many or one is unusable.
 */
function listed(files) {
    if (files.length > MAX_LIST_FILES) {
        throw malformed(`listing holds more than ${MAX_LIST_FILES} files`);
    }
    return files.map(fileVersion);
}
/**
 * Compares two strings by their UTF-8 bytes, the order B2 lists names in.
 *
 * @param a - First string.
 * @param b - Second string.
 * @returns Negative, zero or positive, as `a` sorts before, with or after `b`.
 */
export function compareNames(a, b) {
    const left = ENCODER.encode(a);
    const right = ENCODER.encode(b);
    const n = Math.min(left.length, right.length);
    for (let i = 0; i < n; i++) {
        const diff = (left[i] ?? 0) - (right[i] ?? 0);
        if (diff !== 0)
            return diff;
    }
    return left.length - right.length;
}
/**
 * Reads the reply to `b2_list_file_names`.
 *
 * @param bytes - The reply body.
 * @returns The page.
 * @throws {B2ReplyError} When the reply is unusable or its continuation
 *   does not move past the page.
 */
export function parseNamesPage(bytes) {
    const what = "file name listing";
    const wire = decodeObject(what, bytes);
    const files = listed(objects(wire, "files", what));
    const next = optionalText(wire, "nextFileName", what);
    if (next === null)
        return { files, nextFileName: null };
    const problem = fileNameProblem(next);
    if (problem !== null)
        throw malformed(`next file name: ${problem}`);
    if (files.some((file) => compareNames(file.fileName, next) >= 0)) {
        throw malformed("next file name does not move past the page");
    }
    return { files, nextFileName: next };
}
/**
 * Reads the reply to `b2_list_file_versions`.
 *
 * @param bytes - The reply body.
 * @returns The page.
 * @throws {B2ReplyError} When the reply is unusable, out of order, or its
 *   continuation is half missing or goes backwards.
 */
export function parseVersionsPage(bytes) {
    const what = "file version listing";
    const wire = decodeObject(what, bytes);
    const files = listed(objects(wire, "files", what));
    for (let i = 1; i < files.length; i++) {
        const before = files[i - 1];
        const after = files[i];
        if (!before || !after)
            continue;
        const order = compareNames(before.fileName, after.fileName);
        if (order > 0)
            throw malformed("version listing names go backwards");
        if (order === 0 && before.uploadTimestampMs < after.uploadTimestampMs) {
            throw malformed("a name's versions are not newest first");
        }
    }
    const name = optionalText(wire, "nextFileName", what);
    const id = optionalText(wire, "nextFileId", what);
    if (name === null && id === null)
        return { files, next: null };
    if (name === null || id === null)
        throw malformed("version listing continuation is half missing");
    const problem = fileNameProblem(name);
    if (problem !== null)
        throw malformed(`next file name: ${problem}`);
    if (files.some((file) => compareNames(file.fileName, name) > 0)) {
        throw malformed("next file name goes back before the page");
    }
    return { files, next: { name, fileId: checkId("file id", id) } };
}
/**
 * Reads the reply to `b2_upload_part` or `b2_copy_part`.
 *
 * @param bytes - The reply body.
 * @returns The part.
 * @throws {B2ReplyError} When the reply is unusable.
 */
export function parsePart(bytes) {
    const what = "part reply";
    const wire = decodeObject(what, bytes);
    const partNumber = count(wire, "partNumber", what);
    if (partNumber === 0 || partNumber > MAX_PARTS)
        throw malformed("part number is out of range");
    const contentSha1 = sha1Of(text(wire, "contentSha1", what));
    if (contentSha1 === null)
        throw malformed("part SHA-1 is not hex");
    return { partNumber, contentSha1 };
}
/**
 * Reads a `Content-Range` value.
 *
 * @param value - The header value.
 * @returns Total size, start and span, or `null` when malformed.
 */
function contentRange(value) {
    const match = /^bytes ([0-9]+)-([0-9]+)\/([0-9]+)$/.exec(value);
    if (!match)
        return null;
    const start = Number(match[1]);
    const end = Number(match[2]);
    const size = Number(match[3]);
    if (![start, end, size].every(Number.isSafeInteger) || start > end || end >= size)
        return null;
    return { size, start, span: end - start + 1 };
}
/**
 * Reads a whole decimal number from a header.
 *
 * @param what - The header, for errors.
 * @param value - The header value.
 * @returns The number.
 * @throws {B2ReplyError} When it is not a whole number.
 */
function headerNumber(what, value) {
    const trimmed = value.trim();
    if (!/^\+?[0-9]+$/.test(trimmed))
        throw malformed(`${what} is not a number`);
    const parsed = Number(trimmed);
    if (!Number.isSafeInteger(parsed))
        throw malformed(`${what} is not a number`);
    return parsed;
}
/**
 * Reads the headers of a `HEAD` or `GET` download reply.
 *
 * @param headers - Header name and value pairs, in order.
 * @returns What the reply serves.
 * @throws {B2ReplyError} When a header is missing, repeated or malformed.
 */
export function parseDownloadHeaders(headers) {
    const slots = new Map();
    const info = new Map();
    let seen = 0;
    for (const [rawName, value] of headers) {
        if (seen >= MAX_DOWNLOAD_HEADERS)
            throw malformed("download reply has too many headers");
        seen += 1;
        const name = rawName.toLowerCase();
        const tracked = name === "content-length" ||
            name === "content-range" ||
            name === "content-type" ||
            name.startsWith("x-bz-");
        if (!tracked)
            continue;
        if (name.startsWith("x-bz-info-")) {
            const decoded = decodeFileName(value);
            if (decoded === null)
                throw malformed("file info value is not percent-encoded text");
            const key = name.slice("x-bz-info-".length);
            if (info.has(key))
                throw malformed("file info key is repeated");
            info.set(key, decoded);
            continue;
        }
        if (name === "content-length" ||
            name === "content-range" ||
            name === "content-type" ||
            name === "x-bz-file-id" ||
            name === "x-bz-file-name" ||
            name === "x-bz-content-sha1" ||
            name === "x-bz-upload-timestamp") {
            if (slots.has(name))
                throw malformed(`header ${name} is repeated`);
            slots.set(name, value);
        }
    }
    const lengthText = slots.get("content-length");
    if (lengthText === undefined)
        throw malformed("download reply has no Content-Length");
    const bodyLength = headerNumber("Content-Length", lengthText);
    let total = bodyLength;
    let bodyOffset = 0;
    const rangeText = slots.get("content-range");
    if (rangeText !== undefined) {
        const range = contentRange(rangeText.trim());
        if (range === null)
            throw malformed("Content-Range is malformed");
        if (range.span !== bodyLength)
            throw malformed("Content-Range disagrees with Content-Length");
        total = range.size;
        bodyOffset = range.start;
    }
    const nameText = slots.get("x-bz-file-name");
    if (nameText === undefined)
        throw malformed("download reply has no X-Bz-File-Name");
    const fileName = decodeFileName(nameText);
    if (fileName === null)
        throw malformed("X-Bz-File-Name is not percent-encoded text");
    const stampText = slots.get("x-bz-upload-timestamp");
    const uploadTimestamp = stampText === undefined ? 0 : headerNumber("X-Bz-Upload-Timestamp", stampText);
    const fileId = slots.get("x-bz-file-id");
    if (fileId === undefined)
        throw malformed("download reply has no X-Bz-File-Id");
    const wire = {
        fileId,
        fileName,
        contentLength: total,
        contentSha1: slots.get("x-bz-content-sha1") ?? null,
        contentType: slots.get("content-type") ?? null,
        uploadTimestamp,
        action: "upload",
        fileInfo: Object.fromEntries(info),
    };
    return { version: fileVersion(wire), bodyOffset, bodyLength };
}
/**
 * True when the error says the session token expired or is bad.
 *
 * @param err - The error.
 * @returns Whether to sign in again.
 */
export function expiredToken(err) {
    return err.status === 401 && (err.code === "expired_auth_token" || err.code === "bad_auth_token");
}
/**
 * True when B2 asks the caller to try again.
 *
 * @param err - The error.
 * @returns Whether the call may be retried.
 */
export function retryable(err) {
    return err.status === 408 || err.status === 429 || err.status === 500 || err.status === 503;
}
/**
 * True when the error says the file or version is gone.
 *
 * @param err - The error.
 * @returns Whether it means "not found".
 */
export function notFound(err) {
    return err.status === 404 || err.code === "not_found" || err.code === "file_not_present";
}
/**
 * Prints an error for logs and messages.
 *
 * @param err - The error.
 * @returns One line.
 */
export function describeApiError(err) {
    return err.message === ""
        ? `B2 answered ${err.status} (${err.code})`
        : `B2 answered ${err.status} (${err.code}): ${err.message}`;
}
/**
 * Drops control characters and cuts `value` to {@link MAX_ERROR_TEXT_BYTES}.
 *
 * @param value - The text.
 * @returns The bounded text.
 */
function boundedText(value) {
    let out = "";
    let used = 0;
    for (const char of value) {
        if (hasControl(char))
            continue;
        const size = utf8Length(char);
        if (used + size > MAX_ERROR_TEXT_BYTES)
            break;
        out += char;
        used += size;
    }
    return out;
}
/**
 * Reads an error reply; never fails (an unreadable body gives `unknown`).
 *
 * @param status - HTTP status.
 * @param bytes - The body (may be empty).
 * @returns The error.
 */
export function parseApiError(status, bytes) {
    let code = null;
    let message = null;
    try {
        const wire = decodeObject("error reply", bytes);
        code = wire["code"];
        message = wire["message"];
    }
    catch {
        // An unreadable body still yields the status.
    }
    const codeOk = typeof code === "string" &&
        code.length > 0 &&
        code.length <= MAX_ERROR_TEXT_BYTES &&
        /^[a-z0-9_]+$/.test(code);
    return {
        status,
        code: codeOk && typeof code === "string" ? code : "unknown",
        message: typeof message === "string" ? boundedText(message) : "",
    };
}
