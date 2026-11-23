/**
 * The ngrok ingress's lifecycle and socket handling, apart from the
 * entrypoint classes.
 *
 * workerd treats every named export of a Worker's main module as an
 * entrypoint, so `index.ts` exports only the classes and this module holds
 * the logic they call, which the tests drive with fakes.
 *
 * The agent's connection is one host-held `tls://` stream to ngrok. Its
 * session (muxado streams, windows, half-read requests) lives in this
 * isolate's memory, keyed by the socket's id: it is far larger than a
 * socket attachment may be. When the isolate starts again while the
 * stream is open, the session is gone, so the plugin closes the stream
 * and opens a new one, which authenticates and binds again.
 *
 * A client's WebSocket rides on a socket to the library that
 * `ORIGIN.connect` opened; its attachment names the agent's socket and the
 * stream, so the library's messages and close find the session. When the
 * agent's session ends, those sockets close with it.
 */
import { PluginError, } from "@bookclerk/plugin-sdk/workerd";
import { Session } from "./agent.js";
import { NGROK_ROOT_CA } from "./ca.js";
import { MuxError } from "./mux.js";
import { VERSION } from "./version.js";
/** Where the agent connects: ngrok's agent endpoint. */
export const NGROK_AGENT_URL = "tls://connect.ngrok-agent.com:443";
/** Close code for a stream the plugin closed because the ingress stopped. */
export const CLOSE_STOPPED = 4001;
/** Close code for a session ngrok refused (authtoken, domain) or stopped. */
export const CLOSE_FAILED = 4002;
/** Close code for a stream whose session this isolate no longer holds. */
export const CLOSE_RESTARTED = 4003;
/** Close code for a session that broke the protocol or that ngrok ended. */
export const CLOSE_BROKEN = 4004;
/** Close code for a stream `health` replaced with a new one. */
export const CLOSE_REPLACED = 4005;
/** How long `start` waits for the endpoint to come online, in milliseconds. */
export const START_WAIT_MS = 20_000;
/** How long `health` waits for a heartbeat's echo, in milliseconds. */
export const BEAT_WAIT_MS = 5_000;
/** Pause between checks while `start` or `health` waits, in milliseconds. */
const POLL_MS = 100;
/** Close code for a socket to the library whose client side ended. */
const CLOSE_GOING_AWAY = 1001;
/** Sessions by socket id; lost when the isolate starts again. */
const sessions = new Map();
/** A DNS name: letters, digits and hyphens in dot-separated labels. */
const DOMAIN = /^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
/**
 * Reads the settings.
 *
 * @param env - The plugin's bindings.
 * @returns Domain and authtoken.
 * @throws {PluginError} `invalid_params` when one is missing or the domain
 *   is not a host name.
 */
export function settingsOf(env) {
    const domain = String(env.CONFIG.domain ?? "")
        .trim()
        .toLowerCase()
        .replace(/\.$/, "");
    const authtoken = String(env.SECRETS["authtoken"] ?? "").trim();
    if (!domain) {
        throw PluginError.fromWire("invalid_params", "set domain to your ngrok domain");
    }
    if (!DOMAIN.test(domain)) {
        throw PluginError.fromWire("invalid_params", "domain must be a host name such as example.ngrok-free.app, without https://");
    }
    if (!authtoken) {
        throw PluginError.fromWire("invalid_params", "set the authtoken secret to your ngrok authtoken");
    }
    return { domain, authtoken };
}
/**
 * The domain a socket's session binds.
 *
 * @param ws - The socket.
 * @returns The domain, or `null` for a socket that is not the agent's.
 */
export function domainOf(ws) {
    const attachment = ws.deserializeAttachment();
    return typeof attachment?.domain === "string" ? attachment.domain : null;
}
/**
 * The client stream a socket to the library carries.
 *
 * @param ws - The socket.
 * @returns Its attachment, or `null` for the agent's own socket.
 */
export function liveOf(ws) {
    const attachment = ws.deserializeAttachment();
    return typeof attachment?.agent === "string" && Number.isSafeInteger(attachment.stream)
        ? { agent: attachment.agent, stream: attachment.stream }
        : null;
}
/**
 * Closes the sockets to the library that an agent socket's session carried.
 *
 * @param sockets - The socket binding.
 * @param agent - The agent socket's id.
 * @returns Resolves once each close was sent.
 */
async function closeLive(sockets, agent) {
    for (const socket of await sockets.getWebSockets().catch(() => [])) {
        if (liveOf(socket)?.agent === agent) {
            await socket.close(CLOSE_GOING_AWAY, "the ngrok session ended").catch(() => { });
        }
    }
}
/**
 * What a session call reaches through this invocation's bindings.
 *
 * @param env - The plugin's bindings.
 * @param ws - The agent's socket.
 * @returns The connection and the library.
 */
export function ioFor(env, ws) {
    return {
        write: (bytes) => ws.send(bytes),
        library: {
            fetch: (request, clientAddress) => {
                if (!env.ORIGIN) {
                    return Promise.reject(new Error("the library is not reachable through this ingress"));
                }
                return env.ORIGIN.fetch(request, clientAddress ? { clientAddress } : undefined);
            },
            connect: (request, clientAddress, stream) => {
                if (!env.ORIGIN) {
                    return Promise.reject(new Error("the library is not reachable through this ingress"));
                }
                return env.ORIGIN.connect(request, {
                    attachment: { agent: ws.id, stream },
                    ...(clientAddress ? { clientAddress } : {}),
                });
            },
            socket: (id) => {
                try {
                    return env.SOCKETS.getWebSocket(id);
                }
                catch {
                    return null;
                }
            },
        },
    };
}
/**
 * The session of a socket, for tests.
 *
 * @param id - The socket's id.
 * @returns Its session, if this isolate holds one.
 */
export function sessionOf(id) {
    return sessions.get(id);
}
/**
 * Opens the agent's connection and sends the auth request.
 *
 * @param env - The plugin's bindings.
 * @param settings - Domain and authtoken.
 * @returns The socket and its session.
 */
export async function connectAgent(env, settings) {
    const ws = await env.SOCKETS.connect(NGROK_AGENT_URL, {
        trustAnchors: [NGROK_ROOT_CA],
        attachment: { domain: settings.domain },
    });
    const session = new Session({ ...settings, version: VERSION });
    sessions.set(ws.id, session);
    try {
        await session.start(ioFor(env, ws));
    }
    catch (err) {
        sessions.delete(ws.id);
        await ws.close(CLOSE_BROKEN, "could not write to ngrok").catch(() => { });
        throw err;
    }
    return { ws, session };
}
/**
 * Closes every agent socket.
 *
 * @param sockets - The socket binding.
 * @param code - Close code.
 * @param reason - Close reason.
 * @returns Resolves once each close was sent.
 */
async function closeAll(sockets, code, reason) {
    for (const ws of await sockets.getWebSockets().catch(() => [])) {
        sessions.delete(ws.id);
        await ws.close(code, reason).catch(() => { });
    }
}
/**
 * Waits until a session is online or has failed.
 *
 * @param ctx - Waits.
 * @param session - The session.
 * @param waitMs - How long to wait.
 * @returns Resolves when it is serving, failed or closed, or the time is up.
 */
async function settled(ctx, session, waitMs) {
    const deadline = Date.now() + waitMs;
    while ((session.phase === "authenticating" || session.phase === "binding") &&
        Date.now() < deadline) {
        await ctx.sleep(POLL_MS);
    }
}
/**
 * Connects to ngrok, binds the domain and returns its origin.
 *
 * @param ctx - Bindings and waits.
 * @returns The public origin.
 * @throws {PluginError} `invalid_params` for bad settings; `unavailable`
 *   when ngrok can't be reached, refuses the authtoken or the domain, or
 *   does not answer in time.
 */
export async function startAgent(ctx) {
    const settings = settingsOf(ctx.env);
    await closeAll(ctx.env.SOCKETS, CLOSE_STOPPED, "restarting");
    let connected;
    try {
        connected = await connectAgent(ctx.env, settings);
    }
    catch (err) {
        if (err instanceof PluginError)
            throw err;
        throw PluginError.fromWire("unavailable", "could not connect to ngrok");
    }
    const { ws, session } = connected;
    await settled(ctx, session, ctx.startWaitMs);
    if (session.phase === "serving")
        return { origin: `https://${settings.domain}` };
    const detail = session.error || "ngrok did not bring the endpoint online in time";
    sessions.delete(ws.id);
    await ws.close(CLOSE_FAILED, "endpoint not online").catch(() => { });
    throw PluginError.fromWire("unavailable", detail);
}
/**
 * Reports whether the endpoint serves: the session is bound and ngrok
 * echoes a heartbeat. A missing or failed session is replaced first.
 *
 * @param ctx - Bindings and waits.
 * @returns `serving`, with a detail for the operator when it is not.
 */
export async function agentHealth(ctx) {
    let settings;
    try {
        settings = settingsOf(ctx.env);
    }
    catch (err) {
        return { serving: false, detail: err instanceof Error ? err.message : "bad settings" };
    }
    let ws;
    let session;
    const all = await ctx.env.SOCKETS.getWebSockets();
    for (const socket of all) {
        if (liveOf(socket) !== null)
            continue;
        const held = sessions.get(socket.id);
        if (held && held.phase === "serving" && domainOf(socket) === settings.domain && !session) {
            ws = socket;
            session = held;
            continue;
        }
        sessions.delete(socket.id);
        await socket.close(CLOSE_REPLACED, "replaced").catch(() => { });
    }
    for (const socket of all) {
        // A socket to the library whose session is gone carries nothing now.
        const live = liveOf(socket);
        if (live !== null && live.agent !== ws?.id) {
            await socket.close(CLOSE_GOING_AWAY, "the ngrok session ended").catch(() => { });
        }
    }
    if (!ws || !session) {
        // The host asks only while the ingress is meant to serve: reconnect.
        try {
            ({ ws, session } = await connectAgent(ctx.env, settings));
        }
        catch {
            return { serving: false, detail: "could not connect to ngrok" };
        }
        await settled(ctx, session, ctx.startWaitMs);
        if (session.phase !== "serving") {
            const detail = session.error || "ngrok did not bring the endpoint online in time";
            sessions.delete(ws.id);
            await ws.close(CLOSE_FAILED, "endpoint not online").catch(() => { });
            return { serving: false, detail };
        }
    }
    let sentAt;
    try {
        sentAt = await session.beat(ioFor(ctx.env, ws), ctx.beat());
    }
    catch {
        return { serving: false, detail: "the connection to ngrok closed" };
    }
    const deadline = Date.now() + ctx.beatWaitMs;
    while (session.lastEcho < sentAt && Date.now() < deadline)
        await ctx.sleep(POLL_MS);
    if (session.lastEcho >= sentAt)
        return { serving: true };
    return { serving: false, detail: "ngrok did not answer a heartbeat" };
}
/**
 * Closes the agent's connection; ngrok takes the endpoint offline.
 *
 * @param sockets - The socket binding.
 * @returns Resolves once each close was sent.
 */
export async function stopAgent(sockets) {
    await closeAll(sockets, CLOSE_STOPPED, "ingress stopped");
}
/**
 * Waits for `ms` milliseconds.
 *
 * @param ms - The pause.
 * @returns Resolves after it.
 */
function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}
/**
 * The context for the live bindings.
 *
 * @param env - The plugin's bindings.
 * @returns The context with real waits and random heartbeats.
 */
export function liveContext(env) {
    return {
        env,
        sleep,
        beat: () => crypto.getRandomValues(new Uint8Array(4)),
        startWaitMs: START_WAIT_MS,
        beatWaitMs: BEAT_WAIT_MS,
    };
}
/**
 * Handles bytes ngrok sent.
 *
 * @param env - The plugin's bindings.
 * @param ws - The agent's socket.
 * @param message - The bytes (a stream delivers only binary messages).
 * @returns Resolves once every frame they complete was handled.
 */
export async function handleMessage(env, ws, message) {
    const live = liveOf(ws);
    if (live !== null) {
        await fromLibrary(env, ws, live, message);
        return;
    }
    if (typeof message === "string")
        return;
    const session = sessions.get(ws.id);
    if (!session) {
        // This isolate started after the session did: its state is gone.
        await ws.close(CLOSE_RESTARTED, "agent restarted").catch(() => { });
        return;
    }
    try {
        await session.receive(ioFor(env, ws), new Uint8Array(message));
    }
    catch (err) {
        sessions.delete(ws.id);
        const reason = err instanceof MuxError ? "protocol error" : "agent error";
        await ws.close(CLOSE_BROKEN, reason).catch(() => { });
        await closeLive(env.SOCKETS, ws.id);
        return;
    }
    if (session.phase === "failed") {
        sessions.delete(ws.id);
        await ws.close(CLOSE_FAILED, "endpoint not online").catch(() => { });
        await closeLive(env.SOCKETS, ws.id);
    }
    else if (session.phase === "closed") {
        sessions.delete(ws.id);
        await ws.close(CLOSE_BROKEN, "session ended").catch(() => { });
        await closeLive(env.SOCKETS, ws.id);
    }
}
/**
 * The agent socket a socket to the library rides on, with its session.
 *
 * @param env - The plugin's bindings.
 * @param live - The library socket's attachment.
 * @returns The agent's socket and session, or `null` when this isolate
 *   holds no session for it.
 */
function agentOf(env, live) {
    const session = sessions.get(live.agent);
    if (!session)
        return null;
    try {
        return { ws: env.SOCKETS.getWebSocket(live.agent), session };
    }
    catch {
        return null;
    }
}
/**
 * Passes a library message to the client stream its socket carries, or
 * closes the socket when that stream or its session is gone.
 *
 * @param env - The plugin's bindings.
 * @param ws - The socket to the library.
 * @param live - Its attachment.
 * @param message - The message.
 * @returns Resolves once it was passed on or the socket closed.
 */
async function fromLibrary(env, ws, live, message) {
    const agent = agentOf(env, live);
    let carried = false;
    if (agent !== null) {
        try {
            carried = await agent.session.fromLibrary(ioFor(env, agent.ws), live.stream, message);
        }
        catch {
            // The write to ngrok failed; the agent socket's own close follows.
        }
    }
    if (!carried)
        await ws.close(CLOSE_GOING_AWAY, "the client went away").catch(() => { });
}
/**
 * Handles a socket closing. A socket to the library passes its close to
 * the client. The agent's socket closes the library sockets it carried
 * and reconnects unless the plugin stopped it or ngrok refused the
 * session.
 *
 * @param env - The plugin's bindings.
 * @param ws - The closed socket.
 * @param code - The close code (1000 when ngrok ended the stream).
 * @param reason - The close reason.
 * @returns Resolves once handled.
 */
export async function handleClose(env, ws, code, reason = "") {
    const live = liveOf(ws);
    if (live !== null) {
        const agent = agentOf(env, live);
        await agent?.session
            .libraryClosed(ioFor(env, agent.ws), live.stream, code, reason)
            .catch(() => { });
        return;
    }
    sessions.delete(ws.id);
    await closeLive(env.SOCKETS, ws.id);
    if (code === CLOSE_STOPPED || code === CLOSE_FAILED || code === CLOSE_REPLACED)
        return;
    if (domainOf(ws) === null)
        return;
    try {
        await connectAgent(env, settingsOf(env));
    }
    catch {
        // `health` connects again.
    }
}
