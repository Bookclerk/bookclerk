/**
 * Cloudflare API calls for the guided setup, which reads every answer
 * whatever its status (a token that is already gone answers 404).
 *
 * The envelope is parsed without I/O ({@link parseEnvelope}) so the fuzz
 * target drives the same code.
 */
import { API_BASE } from "./cloudflare.js";
import { SetupError } from "./tokens.js";
/** Longest API answer read, in bytes. */
export const MAX_ANSWER_BYTES = 1024 * 1024;
/**
 * Reads an API answer.
 *
 * @param status - Its HTTP status.
 * @param text - Its body.
 * @returns The result, or the failure's message.
 * @throws {SetupError} When the body is not a JSON API envelope.
 */
export function parseEnvelope(status, text) {
    let value;
    try {
        value = JSON.parse(text);
    }
    catch {
        throw new SetupError(`Cloudflare answered ${status} without JSON`);
    }
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
        throw new SetupError(`Cloudflare answered ${status} without an API envelope`);
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
/**
 * Calls the API with one token and reads its answer, whatever the status.
 *
 * @param fetch - The request function.
 * @param token - The API token, sent only to {@link API_BASE}.
 * @param path - Path and query under {@link API_BASE}.
 * @param init - Method and JSON body.
 * @returns The answer.
 * @throws {SetupError} When the API cannot be reached or the body is not an
 *   API envelope.
 */
export async function answer(fetch, token, path, init = {}) {
    const headers = new Headers({ authorization: `Bearer ${token}` });
    const request = { method: init.method ?? "GET", headers, redirect: "manual" };
    if (init.body !== undefined) {
        headers.set("content-type", "application/json");
        request.body = JSON.stringify(init.body);
    }
    let response;
    try {
        response = await fetch(`${API_BASE}${path}`, request);
    }
    catch {
        throw new SetupError("could not reach the Cloudflare API");
    }
    const text = await readCapped(response, MAX_ANSWER_BYTES);
    if (text === null)
        throw new SetupError(`Cloudflare answered ${response.status} at length`);
    return { status: response.status, ...parseEnvelope(response.status, text) };
}
