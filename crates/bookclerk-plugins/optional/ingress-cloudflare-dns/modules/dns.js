/**
 * Cloudflare DNS through the API: finding the host name's zone and keeping
 * its A and AAAA records.
 *
 * The answers are parsed without I/O ({@link parseEnvelope},
 * {@link parseZone}, {@link parseRecords}) so the fuzz targets drive the
 * same code. Records the plugin writes carry {@link RECORD_COMMENT}; it
 * deletes only records that carry it.
 */
/** Cloudflare API base URL. */
export const API_BASE = "https://api.cloudflare.com/client/v4";
/** The comment on records the plugin keeps. */
export const RECORD_COMMENT = "Kept by Bookclerk (cloudflare_dns)";
/** The records' TTL in seconds: the shortest Cloudflare allows on unproxied records. */
export const RECORD_TTL = 60;
/** Longest API answer read, in bytes. */
export const MAX_ANSWER_BYTES = 1024 * 1024;
/** Why a DNS call failed, for the operator. */
export class DnsError extends Error {
    /**
     * Creates the error.
     *
     * @param message - What went wrong and what to do; never a credential.
     */
    constructor(message) {
        super(message);
        this.name = "DnsError";
    }
}
/** What a 401 or 403 means while the plugin keeps the records. */
export const RECORDS_HINT = "The API token needs the Zone: Zone: Read and Zone: DNS: Edit permissions on the host name's zone.";
/** Cloudflare's ids: 32 lowercase hex digits. */
const ID = /^[0-9a-f]{32}$/;
/**
 * Reads an API answer.
 *
 * @param status - Its HTTP status.
 * @param text - Its body.
 * @returns The result, or the failure's message.
 * @throws {DnsError} When the body is not a JSON API envelope.
 */
export function parseEnvelope(status, text) {
    let value;
    try {
        value = JSON.parse(text);
    }
    catch {
        throw new DnsError(`Cloudflare answered ${status} without JSON`);
    }
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
        throw new DnsError(`Cloudflare answered ${status} without an API envelope`);
    }
    const envelope = value;
    if (status >= 200 && status < 300 && envelope.success === true) {
        return { ok: true, result: envelope.result };
    }
    const first = Array.isArray(envelope.errors) ? envelope.errors[0] : undefined;
    const message = typeof first === "object" && first !== null
        ? first.message
        : undefined;
    return { ok: false, message: typeof message === "string" ? message.slice(0, 200) : "" };
}
/**
 * Finds the zone named `name` in a zone list.
 *
 * @param result - The list's `result`.
 * @param name - The zone name asked for.
 * @returns The zone, or `null` when the list holds no active zone of that
 *   name with a valid id.
 * @throws {DnsError} When the result is not a list.
 */
export function parseZone(result, name) {
    if (!Array.isArray(result))
        throw new DnsError("Cloudflare's zone list is not a list");
    for (const entry of result) {
        if (typeof entry !== "object" || entry === null)
            continue;
        const zone = entry;
        if (typeof zone.id === "string" &&
            ID.test(zone.id) &&
            zone.name === name &&
            zone.status === "active") {
            const owner = typeof zone.account === "object" && zone.account !== null
                ? zone.account.id
                : undefined;
            return {
                id: zone.id,
                account: typeof owner === "string" && ID.test(owner) ? owner : null,
            };
        }
    }
    return null;
}
/**
 * Reads a record list, keeping the records at `name`.
 *
 * @param result - The list's `result`.
 * @param name - The host name.
 * @returns The records named exactly `name` with a valid id, a type and
 *   text content; others are dropped.
 * @throws {DnsError} When the result is not a list.
 */
export function parseRecords(result, name) {
    if (!Array.isArray(result))
        throw new DnsError("Cloudflare's record list is not a list");
    const out = [];
    for (const entry of result) {
        if (typeof entry !== "object" || entry === null)
            continue;
        const record = entry;
        if (typeof record.id !== "string" ||
            !ID.test(record.id) ||
            typeof record.type !== "string" ||
            record.name !== name ||
            typeof record.content !== "string") {
            continue;
        }
        out.push({
            id: record.id,
            type: record.type,
            content: record.content,
            proxied: record.proxied === true,
            ttl: typeof record.ttl === "number" ? record.ttl : 0,
            comment: typeof record.comment === "string" ? record.comment : "",
        });
    }
    return out;
}
/**
 * Reads a body, stopping past `limit` bytes.
 *
 * @param response - The response.
 * @param limit - Most bytes read.
 * @returns The text, or `null` when the body is longer than `limit`.
 */
export async function readCapped(response, limit) {
    if (response.body === null)
        return "";
    const reader = response.body.getReader();
    const chunks = [];
    let size = 0;
    for (;;) {
        const { done, value } = await reader.read();
        if (done)
            break;
        size += value.length;
        if (size > limit) {
            await reader.cancel().catch(() => { });
            return null;
        }
        chunks.push(value);
    }
    const joined = new Uint8Array(size);
    let at = 0;
    for (const chunk of chunks) {
        joined.set(chunk, at);
        at += chunk.length;
    }
    return new TextDecoder().decode(joined);
}
/** The Cloudflare API, as one token reaches it. */
export class CloudflareDns {
    /** Makes the requests. */
    #fetch;
    /** The API token, sent only to {@link API_BASE}. */
    #token;
    /** What a 401 or 403 tells the operator to change. */
    #hint;
    /**
     * Creates the client.
     *
     * @param fetch - The request function.
     * @param token - The API token.
     * @param hint - What a 401 or 403 tells the operator to change.
     */
    constructor(fetch, token, hint = RECORDS_HINT) {
        this.#fetch = fetch;
        this.#token = token;
        this.#hint = hint;
    }
    /**
     * Calls the API and reads its answer, whatever the status.
     *
     * @param path - Path and query under {@link API_BASE}.
     * @param init - Method and JSON body.
     * @returns The answer.
     * @throws {DnsError} When the API cannot be reached or the body is not an
     *   API envelope.
     */
    async answer(path, init = {}) {
        const headers = new Headers({ authorization: `Bearer ${this.#token}` });
        const request = { method: init.method ?? "GET", headers, redirect: "manual" };
        if (init.body !== undefined) {
            headers.set("content-type", "application/json");
            request.body = JSON.stringify(init.body);
        }
        let response;
        try {
            response = await this.#fetch(`${API_BASE}${path}`, request);
        }
        catch {
            throw new DnsError("could not reach the Cloudflare API");
        }
        const text = await readCapped(response, MAX_ANSWER_BYTES);
        if (text === null)
            throw new DnsError(`Cloudflare answered ${response.status} at length`);
        return { status: response.status, ...parseEnvelope(response.status, text) };
    }
    /**
     * Calls the API.
     *
     * @param path - Path and query under {@link API_BASE}.
     * @param init - Method and JSON body.
     * @returns The `result` member.
     * @throws {DnsError} When the call fails.
     */
    async #call(path, init = {}) {
        const answer = await this.answer(path, init);
        if (answer.ok)
            return answer.result;
        const said = answer.message
            ? `Cloudflare answered ${answer.status}: ${answer.message}`
            : `Cloudflare answered ${answer.status}`;
        if (answer.status === 401 || answer.status === 403)
            throw new DnsError(`${said}. ${this.#hint}`);
        throw new DnsError(said);
    }
    /**
     * Finds the zone a host name is in.
     *
     * @param candidates - Zone names to try, longest first.
     * @returns The zone.
     * @throws {DnsError} When no candidate is an active zone the token reaches.
     */
    async zone(candidates) {
        for (const name of candidates) {
            const result = await this.#call(`/zones?name=${encodeURIComponent(name)}&status=active`);
            const zone = parseZone(result, name);
            if (zone !== null)
                return zone;
        }
        throw new DnsError(`no active Cloudflare zone holds ${candidates[0] ?? "the host name"}: add its domain to Cloudflare, or give the API token access to its zone`);
    }
    /**
     * Lists the records at a name.
     *
     * @param zone - The zone's id.
     * @param name - The host name.
     * @returns Its records.
     * @throws {DnsError} When the list cannot be read.
     */
    async records(zone, name) {
        const result = await this.#call(`/zones/${zone}/dns_records?name=${encodeURIComponent(name)}&per_page=100`);
        return parseRecords(result, name);
    }
    /**
     * Creates an address record.
     *
     * @param zone - The zone's id.
     * @param name - The host name.
     * @param type - `A` or `AAAA`.
     * @param content - The address.
     * @returns Resolves once created.
     * @throws {DnsError} When Cloudflare refuses.
     */
    async create(zone, name, type, content) {
        await this.#call(`/zones/${zone}/dns_records`, {
            method: "POST",
            body: { type, name, content, ttl: RECORD_TTL, proxied: false, comment: RECORD_COMMENT },
        });
    }
    /**
     * Points an address record at an address, unproxied, and marks it as
     * kept by the plugin.
     *
     * @param zone - The zone's id.
     * @param id - The record's id.
     * @param content - The address.
     * @returns Resolves once updated.
     * @throws {DnsError} When Cloudflare refuses.
     */
    async update(zone, id, content) {
        await this.#call(`/zones/${zone}/dns_records/${id}`, {
            method: "PATCH",
            body: { content, ttl: RECORD_TTL, proxied: false, comment: RECORD_COMMENT },
        });
    }
    /**
     * Deletes a record.
     *
     * @param zone - The zone's id.
     * @param id - The record's id.
     * @returns Resolves once deleted.
     * @throws {DnsError} When Cloudflare refuses.
     */
    async remove(zone, id) {
        await this.#call(`/zones/${zone}/dns_records/${id}`, { method: "DELETE" });
    }
}
