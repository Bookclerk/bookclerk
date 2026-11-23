/**
 * Generic webhook notifier (workerd guest).
 *
 * Exports the `notifier` entrypoint `plugin.toml` declares. Each send is one
 * signed POST, planned by `webhook.ts`: to the instance's `url` setting for a
 * channel, or to the URL in a patron's address. This module only makes the
 * call and adapts typed failures to `PluginError`. Nothing here logs: a URL
 * may hold a token and a message may be sensitive.
 */
import { BookclerkEntrypoint, NotifierEntrypoint, PluginError, } from "@bookclerk/plugin-sdk/workerd";
import { VERSION } from "./version.js";
import { SEND_TIMEOUT_MS, WebhookFailure, channelEndpoint, checkResponse, parseAddress, plan, } from "./webhook.js";
/**
 * Runs `call`, turning a `WebhookFailure` into a `PluginError` with its code.
 *
 * @param call - The work.
 * @returns What `call` returns.
 * @throws {PluginError} When `call` fails.
 */
async function guarded(call) {
    try {
        return await call();
    }
    catch (err) {
        if (err instanceof WebhookFailure) {
            throw new PluginError(err.code, err.message, err.retryAfterMs === undefined ? undefined : { retryAfterMs: err.retryAfterMs });
        }
        throw err;
    }
}
/** `notifier` entrypoint: one signed POST per message. */
export class Notifier extends NotifierEntrypoint {
    /**
     * The channel endpoint from this instance's settings.
     *
     * @returns The endpoint.
     * @throws {WebhookFailure} When the settings are missing or malformed.
     */
    channel() {
        return channelEndpoint(this.env.CONFIG.url, this.env.SECRETS["signing_secret"]);
    }
    /**
     * Checks the channel settings without a request, so a probe never posts.
     *
     * @returns Ready when no channel URL is set or the settings are valid.
     */
    health() {
        const url = this.env.CONFIG.url;
        if (url === undefined || url.trim() === "") {
            return Promise.resolve({ ok: true, detail: "no channel URL; patrons' endpoints only" });
        }
        try {
            this.channel();
            return Promise.resolve({ ok: true, detail: "" });
        }
        catch (err) {
            if (err instanceof WebhookFailure)
                return Promise.resolve({ ok: false, detail: err.message });
            throw err;
        }
    }
    /**
     * Posts one message to one destination.
     *
     * @param params - The rendered message and its destination.
     * @returns Nothing from the receiver; Standard Webhooks has no message id.
     */
    send(params) {
        return guarded(async () => {
            const { message, destination } = params;
            const endpoint = destination.kind === "personal"
                ? parseAddress(destination.addressJson).endpoint
                : this.channel();
            const request = await plan(endpoint, message, destination.destinationId, new Date(), VERSION);
            let response;
            try {
                response = await fetch(request.url, {
                    method: "POST",
                    headers: request.headers,
                    body: request.body,
                    redirect: "manual",
                    signal: AbortSignal.timeout(SEND_TIMEOUT_MS),
                });
            }
            catch {
                throw new WebhookFailure("unavailable", "the receiver could not be reached");
            }
            // The body is not needed; drop it so the connection can be reused.
            await response.body?.cancel().catch(() => undefined);
            checkResponse(response.status, response.headers.get("retry-after"), Date.now());
            return {};
        });
    }
    /**
     * Checks a patron's endpoint without a request.
     *
     * @param params - The address document as entered.
     * @returns The trimmed document and the endpoint's host as its label.
     */
    validateDestination(params) {
        return guarded(() => {
            const address = parseAddress(params.addressJson);
            return Promise.resolve({ addressJson: address.addressJson, label: address.label });
        });
    }
}
/** Identity the host merges over `plugin.toml`. */
export default class WebhookPlugin extends BookclerkEntrypoint {
    /**
     * Describes the plugin.
     *
     * @returns Display name.
     */
    async describe() {
        return { displayName: "Webhook" };
    }
}
