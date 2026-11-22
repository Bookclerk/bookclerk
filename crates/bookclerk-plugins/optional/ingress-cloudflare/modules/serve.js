/**
 * Serves one relayed request: pull it from the relay, hand it to the
 * library through `env.ORIGIN`, push the library's answer back.
 *
 * The whole exchange runs inside the socket message that announced it, so
 * every stream it touches belongs to that one invocation. The relay sends
 * a socket no new notice until the push is done.
 */
import { framed, readFrame } from "./relay-core.js";
import { HOP_BY_HOP, RELAY_PULL_PATH, RELAY_PUSH_PATH, encodeFrame, parseRequestHead, } from "./protocol.js";
/** Statuses whose responses carry no body. */
const NULL_BODY = new Set([101, 204, 205, 304]);
/**
 * The library's response head as the relay takes it.
 *
 * @param response - The library's response.
 * @returns Status and header fields; informational statuses become 502.
 */
export function responseHead(response) {
    const headers = [];
    response.headers.forEach((value, name) => {
        const lower = name.toLowerCase();
        if (HOP_BY_HOP.has(lower))
            return;
        headers.push({ name: lower, value });
    });
    // Set-Cookie fields stay separate rather than joined by `forEach`.
    const cookies = typeof response.headers.getSetCookie === "function" ? response.headers.getSetCookie() : [];
    if (cookies.length > 1) {
        const rest = headers.filter((field) => field.name !== "set-cookie");
        for (const cookie of cookies)
            rest.push({ name: "set-cookie", value: cookie });
        return { status: statusOf(response.status), headers: rest };
    }
    return { status: statusOf(response.status), headers };
}
/**
 * A status the relay can answer with.
 *
 * @param status - The library's status.
 * @returns It, or 502 for one outside 200 to 599.
 */
function statusOf(status) {
    return status >= 200 && status <= 599 ? status : 502;
}
/**
 * Pushes an answer to the relay.
 *
 * @param ctx - Relay and token.
 * @param id - The request's id.
 * @param head - The response head.
 * @param body - The response body, or `null`.
 * @returns Whether the relay took it.
 */
async function push(ctx, id, head, body) {
    try {
        const response = await ctx.fetch(`https://${ctx.host}${RELAY_PUSH_PATH}${id}`, {
            method: "POST",
            headers: { authorization: `Bearer ${ctx.token}`, "content-type": "application/octet-stream" },
            body: framed(encodeFrame(head), body),
            redirect: "manual",
            duplex: "half",
        });
        await response.body?.cancel().catch(() => { });
        return response.ok;
    }
    catch {
        await body?.cancel().catch(() => { });
        return false;
    }
}
/**
 * A plain error answer for the public client.
 *
 * @param status - HTTP status.
 * @param message - What happened.
 * @returns The head and body.
 */
function errorAnswer(status, message) {
    const bytes = new TextEncoder().encode(JSON.stringify({ error: "library_unavailable", message, status }));
    return {
        head: { status, headers: [{ name: "content-type", value: "application/json" }] },
        body: new ReadableStream({
            start(controller) {
                controller.enqueue(bytes);
                controller.close();
            },
        }),
    };
}
/**
 * Serves one relayed request.
 *
 * @param ctx - The relay, its token and the library.
 * @param id - The request's id, from the relay's notice.
 * @returns How it ended.
 */
export async function serveRequest(ctx, id) {
    let pulled;
    try {
        pulled = await ctx.fetch(`https://${ctx.host}${RELAY_PULL_PATH}${id}`, {
            headers: { authorization: `Bearer ${ctx.token}` },
            redirect: "manual",
        });
    }
    catch {
        return "gone";
    }
    if (!pulled.ok || !pulled.body) {
        await pulled.body?.cancel().catch(() => { });
        return "gone";
    }
    const frame = await readFrame(pulled.body);
    if (typeof frame === "string") {
        const answer = errorAnswer(502, "The relay sent a malformed request.");
        await push(ctx, id, answer.head, answer.body);
        return "bad_request";
    }
    const head = parseRequestHead(frame.json);
    if (typeof head === "string") {
        await frame.body.cancel().catch(() => { });
        const answer = errorAnswer(400, "The request could not be passed to the library.");
        await push(ctx, id, answer.head, answer.body);
        return "bad_request";
    }
    const bodyless = head.method === "GET" || head.method === "HEAD";
    if (bodyless)
        await frame.body.cancel().catch(() => { });
    const headers = new Headers();
    for (const field of head.headers)
        headers.append(field.name, field.value);
    const request = new Request(`https://${ctx.host}${head.target}`, {
        method: head.method,
        headers,
        body: bodyless ? null : frame.body,
        duplex: "half",
    });
    let response;
    try {
        response = await ctx.origin.fetch(request, head.client ? { clientAddress: head.client } : undefined);
    }
    catch {
        if (!bodyless)
            await frame.body.cancel().catch(() => { });
        const answer = errorAnswer(502, "The library could not serve the request.");
        await push(ctx, id, answer.head, answer.body);
        return "library_failed";
    }
    const body = NULL_BODY.has(response.status) || head.method === "HEAD" ? null : response.body;
    if (!body)
        await response.body?.cancel().catch(() => { });
    return (await push(ctx, id, responseHead(response), body)) ? "served" : "push_failed";
}
