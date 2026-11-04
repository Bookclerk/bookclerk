/**
 * Open Library request building and response mapping, without I/O.
 *
 * Every function here is pure: the guest entrypoint and the unit tests drive
 * the same code, and the only network call lives in `client.ts`. Responses
 * are untrusted JSON, so each field is checked for its type before it is
 * copied into a `MetadataRecord`, and every list is capped.
 *
 * Open Library asks API users to identify themselves, to keep the request
 * rate low, and to prefer `search.json` over many single-book requests
 * (<https://openlibrary.org/developers/api>). One provider call therefore
 * makes at most one request, and it always goes to `search.json` with an
 * explicit field list.
 */
/** Origin every request goes to; `plugin.toml` grants only this host. */
export const ORIGIN = "https://openlibrary.org";
/** Origin of the cover images a record links to (not fetched by the guest). */
export const COVERS_ORIGIN = "https://covers.openlibrary.org";
/** Records a call returns when the caller sets no `limit`. */
export const DEFAULT_LIMIT = 5;
/** Most records one call returns, whatever `limit` asks for. */
export const MAX_LIMIT = 20;
/** Most subject headings copied into `categories`. */
export const MAX_SUBJECTS = 24;
/** Most ISBNs and edition ids copied into a record's identifiers. */
export const MAX_IDS_PER_SCHEME = 10;
/** Most authors copied into a record's credits. */
export const MAX_AUTHORS = 10;
/** Longest text kept from any one response field, in UTF-16 code units. */
export const MAX_TEXT = 4096;
/**
 * Fields requested from `search.json`. Asking for a fixed list keeps the
 * response small. A hit is a work; the `editions.*` fields bring the one
 * edition that best matches the query (the edition an ISBN or edition id
 * names), so edition facts such as language and publisher come from that
 * edition rather than from a list mixing every edition of the work.
 */
export const SEARCH_FIELDS = [
    "key",
    "title",
    "subtitle",
    "author_name",
    "first_sentence",
    "cover_i",
    "subject",
    "first_publish_year",
    "ratings_average",
    "ratings_count",
    "editions",
    "editions.key",
    "editions.title",
    "editions.subtitle",
    "editions.cover_i",
    "editions.isbn",
    "editions.language",
    "editions.publisher",
    "editions.publish_date",
];
/** A failure the entrypoint turns into a `PluginError` with the same code. */
export class ProviderFailure extends Error {
    /** `PluginErrorCode` wire string (`invalid_params`, `unavailable`, ...). */
    code;
    /**
     * Creates a failure.
     *
     * @param code - `PluginErrorCode` wire string the host receives.
     * @param message - Operator-facing explanation; never holds a secret.
     */
    constructor(code, message) {
        super(message);
        this.name = "ProviderFailure";
        this.code = code;
    }
}
/**
 * Clamps a caller's `limit` to `1..=MAX_LIMIT`.
 *
 * @param limit - Requested record count; absent or 0 means the default.
 * @returns The record count to ask for.
 */
export function clampLimit(limit) {
    if (limit === undefined || !Number.isFinite(limit) || limit <= 0) {
        return DEFAULT_LIMIT;
    }
    return Math.min(Math.floor(limit), MAX_LIMIT);
}
/**
 * Normalises an ISBN-10 or ISBN-13 to its bare digits (and a final `X`).
 *
 * Hyphens and spaces are dropped. The check digit is not verified: Open
 * Library matches the digits it has, and a wrong check digit only finds
 * nothing.
 *
 * @param raw - ISBN as the caller printed it.
 * @returns The bare ISBN, or `undefined` when it is not 10 or 13 characters
 *   of the right shape.
 */
export function normalizeIsbn(raw) {
    const bare = raw.replace(/[\s-]/g, "").toUpperCase();
    if (/^\d{9}[\dX]$/.test(bare) || /^\d{13}$/.test(bare)) {
        return bare;
    }
    return undefined;
}
/**
 * Normalises an Open Library id of the given kind (`OL123M` or `OL123W`).
 *
 * Accepts the bare id or its path form (`/books/OL123M`, `/works/OL123W`).
 *
 * @param raw - Id as the caller printed it.
 * @param suffix - `M` for an edition, `W` for a work.
 * @returns The bare id, or `undefined` when it is not one.
 */
export function normalizeOlid(raw, suffix) {
    const last = raw.trim().split("/").pop() ?? "";
    const id = last.toUpperCase();
    return new RegExp(`^OL\\d{1,12}${suffix}$`).test(id) ? id : undefined;
}
/**
 * Builds a `search.json` URL from query parameters.
 *
 * @param params - Query parameters in order; `fields` and `limit` are added.
 * @param limit - Records to ask for.
 * @returns The full URL.
 */
function searchUrl(params, limit) {
    const query = new URLSearchParams(params);
    query.set("fields", SEARCH_FIELDS.join(","));
    query.set("limit", String(limit));
    return `${ORIGIN}/search.json?${query.toString()}`;
}
/**
 * Plans the single request a `lookup` makes.
 *
 * The strongest identifier wins: an edition id, then a work id, then an
 * ISBN. Schemes Open Library does not serve (ASIN) are ignored, as the ABI
 * asks.
 *
 * @param params - Lookup params from the host.
 * @returns The request, or `undefined` when no identifier is one Open
 *   Library serves (the lookup then answers with no records).
 * @throws {ProviderFailure} `invalid_params` when the identifier list is empty.
 */
export function planLookup(params) {
    if (params.identifiers.length === 0) {
        throw new ProviderFailure("invalid_params", "lookup needs at least one identifier");
    }
    const limit = clampLimit(params.limit);
    const find = (scheme, norm) => {
        for (const id of params.identifiers) {
            if (id.scheme === scheme) {
                const value = norm(id.value);
                if (value !== undefined)
                    return value;
            }
        }
        return undefined;
    };
    const edition = find("openLibraryEdition", (v) => normalizeOlid(v, "M"));
    if (edition !== undefined) {
        return { url: searchUrl([["q", `edition_key:${edition}`]], limit), limit };
    }
    const work = find("openLibraryWork", (v) => normalizeOlid(v, "W"));
    if (work !== undefined) {
        return { url: searchUrl([["q", `key:"/works/${work}"`]], limit), limit };
    }
    const isbn = find("isbn", normalizeIsbn);
    if (isbn !== undefined) {
        return { url: searchUrl([["isbn", isbn]], limit), limit };
    }
    return undefined;
}
/**
 * Plans the single request a `search` makes.
 *
 * Open Library ranks the hits; only hits whose title overlaps the asked
 * title are kept, so a loose author match cannot stand in for the book.
 *
 * @param params - Search params from the host.
 * @returns The request.
 * @throws {ProviderFailure} `invalid_params` when the title is blank.
 */
export function planSearch(params) {
    const title = params.title.trim();
    if (title === "") {
        throw new ProviderFailure("invalid_params", "search needs a title");
    }
    const query = [["title", title]];
    const author = params.author?.trim();
    if (author)
        query.push(["author", author]);
    const language = params.language?.trim().toLowerCase();
    if (language && /^[a-z]{2,3}$/.test(language))
        query.push(["lang", language]);
    const limit = clampLimit(params.limit);
    return { url: searchUrl(query, limit), limit, titleFilter: title.toLowerCase() };
}
/**
 * Builds the `User-Agent` Open Library asks API users to send.
 *
 * @param version - Plugin version from `plugin.toml`.
 * @param contactEmail - Operator contact address from the `contact_email`
 *   setting, when set and plausible.
 * @returns The header value.
 */
export function userAgent(version, contactEmail) {
    const email = typeof contactEmail === "string" ? contactEmail.trim() : "";
    // A control character or a parenthesis would break the header's comment.
    const ok = /^[^\s@()<>,;:"\\[\]\p{Cc}]+@[^\s@()<>,;:"\\[\]\p{Cc}]+$/u.test(email);
    const contact = ok ? `${email}; ` : "";
    return `Bookclerk-OpenLibrary/${version} (${contact}https://bookclerk.app)`;
}
/**
 * Returns `value` when it is a non-blank string, trimmed and capped.
 *
 * @param value - Untrusted JSON value.
 * @returns The text, or `undefined`.
 */
function text(value) {
    if (typeof value !== "string")
        return undefined;
    const trimmed = value.trim();
    return trimmed === "" ? undefined : trimmed.slice(0, MAX_TEXT);
}
/**
 * Returns the non-blank strings of `value` when it is an array, capped.
 *
 * @param value - Untrusted JSON value.
 * @param max - Most entries to keep.
 * @returns The strings, deduplicated in order.
 */
function texts(value, max) {
    if (!Array.isArray(value))
        return [];
    const out = [];
    for (const item of value) {
        const s = text(item);
        if (s !== undefined && !out.includes(s)) {
            out.push(s);
            if (out.length >= max)
                break;
        }
    }
    return out;
}
/**
 * Returns `value` when it is a finite number in `min..=max`.
 *
 * @param value - Untrusted JSON value.
 * @param min - Lowest accepted value.
 * @param max - Highest accepted value.
 * @returns The number, or `undefined`.
 */
function numberIn(value, min, max) {
    return typeof value === "number" && Number.isFinite(value) && value >= min && value <= max
        ? value
        : undefined;
}
/**
 * Returns `value` when it is a JSON object.
 *
 * @param value - Untrusted JSON value.
 * @returns The object, or `undefined`.
 */
function object(value) {
    return typeof value === "object" && value !== null && !Array.isArray(value)
        ? value
        : undefined;
}
/**
 * The edition Open Library matched within a hit, when it sent one.
 *
 * @param doc - One hit.
 * @returns The first `editions.docs` entry that is an object.
 */
function matchedEdition(doc) {
    const docs = object(doc.editions)?.docs;
    return Array.isArray(docs) ? object(docs[0]) : undefined;
}
/**
 * Builds a cover URL from an Open Library cover id.
 *
 * @param value - Untrusted `cover_i` value.
 * @returns The large cover URL, or `undefined` when the id is not one.
 */
function coverUrl(value) {
    const id = numberIn(value, 1, Number.MAX_SAFE_INTEGER);
    return id !== undefined && Number.isInteger(id) ? `${COVERS_ORIGIN}/b/id/${id}-L.jpg` : undefined;
}
/**
 * Maps one `search.json` hit to a record.
 *
 * With a matched edition the record describes that edition: its id, ISBNs,
 * title, cover, language, publisher and date. Without one it describes the
 * work and carries only the work id. Authors, subjects, the first sentence
 * and ratings are work facts either way.
 *
 * @param doc - One entry of the response's `docs` array.
 * @returns The record, or `undefined` when the hit has no title or no work
 *   key (Open Library's search only returns works, so a hit without one is
 *   malformed).
 */
export function recordFromDoc(doc) {
    const d = object(doc);
    if (d === undefined)
        return undefined;
    const work = typeof d.key === "string" ? normalizeOlid(d.key, "W") : undefined;
    const workTitle = text(d.title);
    if (work === undefined || workTitle === undefined)
        return undefined;
    const ed = matchedEdition(d);
    const edition = typeof ed?.key === "string" ? normalizeOlid(ed.key, "M") : undefined;
    const identifiers = [{ scheme: "openLibraryWork", value: work }];
    if (edition !== undefined)
        identifiers.push({ scheme: "openLibraryEdition", value: edition });
    for (const raw of texts(ed?.isbn, MAX_IDS_PER_SCHEME)) {
        const isbn = normalizeIsbn(raw);
        if (isbn !== undefined)
            identifiers.push({ scheme: "isbn", value: isbn });
    }
    const credits = texts(d.author_name, MAX_AUTHORS).map((name) => ({
        name,
        role: "author",
        level: "roleDefault",
    }));
    const title = text(ed?.title) ?? workTitle;
    const record = {
        title,
        url: edition !== undefined ? `${ORIGIN}/books/${edition}` : `${ORIGIN}/works/${work}`,
        abridgement: "unknown",
        workKind: "book",
        expressionKind: "text",
        mediaKind: "print",
        identifiers,
        credits,
        seriesEntries: [],
    };
    const subtitle = ed !== undefined ? text(ed.subtitle) : text(d.subtitle);
    if (subtitle !== undefined && subtitle !== title)
        record.subtitle = subtitle;
    const sentence = Array.isArray(d.first_sentence) ? d.first_sentence[0] : d.first_sentence;
    const description = text(sentence);
    if (description !== undefined)
        record.description = description;
    const cover = coverUrl(ed?.cover_i) ?? coverUrl(d.cover_i);
    if (cover !== undefined)
        record.coverUrl = cover;
    const publisher = texts(ed?.publisher, 1)[0];
    if (publisher !== undefined)
        record.publisher = publisher;
    const year = numberIn(d.first_publish_year, 1, 9999);
    const published = texts(ed?.publish_date, 1)[0] ??
        (year !== undefined && Number.isInteger(year) ? String(year) : undefined);
    if (published !== undefined)
        record.publishedAt = published;
    const subjects = texts(d.subject, MAX_SUBJECTS);
    if (subjects.length > 0)
        record.categories = subjects.join("; ");
    const language = texts(ed?.language, 1)[0];
    if (language !== undefined)
        record.language = language;
    const rating = numberIn(d.ratings_average, 0, 5);
    if (rating !== undefined && rating > 0)
        record.ratingOverall = rating;
    const ratingCount = numberIn(d.ratings_count, 1, 0xffff_ffff);
    if (ratingCount !== undefined && Number.isInteger(ratingCount))
        record.ratingCount = ratingCount;
    return record;
}
/**
 * Whether two titles overlap: either contains the other, ignoring case.
 *
 * @param wanted - Lower-cased title the caller asked for.
 * @param title - A hit's title.
 * @returns `true` when the hit plausibly names the asked book.
 */
export function titlesOverlap(wanted, title) {
    const have = title.toLowerCase();
    return have.includes(wanted) || wanted.includes(have);
}
/**
 * Maps a `search.json` response body to records.
 *
 * @param body - Parsed JSON body.
 * @param request - The request that produced it (limit and title filter).
 * @returns Records in Open Library's order, at most `request.limit`.
 * @throws {ProviderFailure} `unavailable` when the body is not a search
 *   response.
 */
export function recordsFromSearch(body, request) {
    const docs = object(body)?.docs;
    if (object(body) === undefined) {
        throw new ProviderFailure("unavailable", "Open Library sent a response that is not a search result");
    }
    if (docs === undefined)
        return [];
    if (!Array.isArray(docs)) {
        throw new ProviderFailure("unavailable", "Open Library sent a search result without a docs list");
    }
    const out = [];
    for (const doc of docs) {
        const record = recordFromDoc(doc);
        if (record === undefined)
            continue;
        if (request.titleFilter !== undefined && !titlesOverlap(request.titleFilter, record.title)) {
            continue;
        }
        out.push(record);
        if (out.length >= request.limit)
            break;
    }
    return out;
}
