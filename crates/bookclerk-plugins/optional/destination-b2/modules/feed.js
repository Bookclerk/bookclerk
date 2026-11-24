/**
 * The destination's change feed: B2 event notifications posted to the
 * host's webhook.
 *
 * B2 posts a bucket's object events to a webhook URL, signed with a shared
 * secret. The host serves the `[[webhooks]]` row, checks the signature and
 * keeps each request until the guest acknowledges it; the SDK's
 * `QueueFeed` runs the receive / acknowledge / retry cycle over it, and
 * `b2-events.ts` reads the bodies.
 *
 * The feed is on only when the operator sets a signing secret and the host
 * has a webhook URL (the library is reachable from the Internet). It then
 * keeps three rules on the bucket for the destination's prefix and checks
 * them again every {@link RULE_RECHECK_MS}, or at once when the URL or
 * secret changes. A page after the rules were (re)written, or after they
 * could not be checked, is incomplete, so the host lists. When the feed is
 * turned off, or the URL goes away, the rules it wrote are removed.
 *
 * What the feed last did to the rules is kept in KV, because the isolate
 * keeps nothing between calls.
 */
import { NO_FEED_CURSOR, QueueFeed, webhookQueue, } from "@bookclerk/plugin-sdk/workerd";
import { parseB2Events } from "./b2-events.js";
import { sha256Hex } from "./hash.js";
/** How long rules that were in place are trusted before checking again: one hour. */
export const RULE_RECHECK_MS = 60 * 60 * 1000;
/**
 * Reads a saved rule state.
 *
 * @param text - The stored text, or `null`.
 * @returns The state; an unreadable one reads as nothing installed.
 */
function readRuleState(text) {
    const empty = { digest: "", checkedAtMs: 0, installed: false };
    if (text === null)
        return empty;
    try {
        const value = JSON.parse(text);
        if (typeof value !== "object" || value === null)
            return empty;
        const { digest, checkedAtMs, installed } = value;
        if (typeof digest !== "string" ||
            typeof checkedAtMs !== "number" ||
            typeof installed !== "boolean") {
            return empty;
        }
        return { digest, checkedAtMs, installed };
    }
    catch {
        return empty;
    }
}
/** B2's change feed; see the module docs. */
export class B2ChangeFeed {
    /** The options. */
    #options;
    /** KV key of the rule state, named after the bucket and prefix. */
    #rulesKey;
    /** The queue feed, when the feed is on. */
    #feed;
    /**
     * Creates the feed; nothing is sent until the first page.
     *
     * @param options - Store, bindings and secret.
     */
    constructor(options) {
        this.#options = options;
        const { store, webhooks, secret, webhook, kv } = options;
        const tag = sha256Hex(`${store.bucket}\n${store.prefix}`).then((digest) => digest.slice(0, 16));
        this.#rulesKey = tag.then((t) => `b2-rules-${t}`);
        if (webhooks === undefined || secret === undefined) {
            this.#feed = null;
            return;
        }
        const scope = { bucket: store.bucket, prefix: store.prefix };
        this.#feed = tag.then((t) => new QueueFeed({
            queue: webhookQueue(webhooks, webhook),
            parse: (body) => parseB2Events(body, scope).map((event) => ({
                change: event.change,
                time: event.time,
                sequencer: null,
            })),
            kv,
            stateKey: `b2-feed-${webhook}-${t}`,
        }));
    }
    /**
     * One page of changes, or `null` when the feed is off (the host lists).
     *
     * @param cursor - The previous page's cursor.
     * @param limit - Requested changes.
     * @returns The page.
     */
    async changes(cursor, limit) {
        const { webhooks, webhook, secret } = this.#options;
        if (this.#feed === null || webhooks === undefined || secret === undefined) {
            await this.#withdrawRules();
            return null;
        }
        const url = await webhooks.url(webhook);
        if (url === null) {
            await this.#withdrawRules();
            return null;
        }
        const steady = await this.#keepRules(url, secret);
        // A page from the listing fallback carries the "no feed" cursor; to the
        // queue feed it is a new start.
        const from = cursor === NO_FEED_CURSOR ? undefined : cursor;
        const page = await (await this.#feed).changes(from, limit);
        return { ...page, complete: page.complete && steady };
    }
    /**
     * Makes sure the rules post to `url`.
     *
     * @param url - The webhook URL.
     * @param secret - The signing secret.
     * @returns `true` when they were already in place (the page may be
     *   complete), `false` when they were rewritten or could not be checked.
     */
    async #keepRules(url, secret) {
        const { store, kv } = this.#options;
        const digest = await sha256Hex([store.bucket, store.prefix, url, secret.expose()].join("\n"));
        const now = (this.#options.now ?? Date.now)();
        const key = await this.#rulesKey;
        const state = readRuleState(await kv.get(key));
        if (state.installed && state.digest === digest && now - state.checkedAtMs < RULE_RECHECK_MS) {
            return true;
        }
        let rewritten;
        try {
            rewritten = await store.keepNotificationRules(url, secret);
        }
        catch (err) {
            console.warn(`B2 event notification rules could not be set; listing instead: ${err instanceof Error ? err.message : String(err)}`);
            return false;
        }
        await this.#saveRules({ digest, checkedAtMs: now, installed: true });
        return !rewritten;
    }
    /**
     * Removes the rules this feed wrote, when it wrote any.
     *
     * @returns Resolves once tried.
     */
    async #withdrawRules() {
        const key = await this.#rulesKey;
        if (!readRuleState(await this.#options.kv.get(key)).installed)
            return;
        try {
            await this.#options.store.dropNotificationRules();
        }
        catch (err) {
            console.warn(`B2 event notification rules not removed: ${err instanceof Error ? err.message : String(err)}`);
            return;
        }
        await this.#saveRules({ digest: "", checkedAtMs: 0, installed: false });
    }
    /**
     * Saves the rule state; a failed save only means checking again sooner.
     *
     * @param state - The state.
     * @returns Resolves once tried.
     */
    async #saveRules(state) {
        try {
            await this.#options.kv.put(await this.#rulesKey, JSON.stringify(state));
        }
        catch (err) {
            console.warn(`B2 notification rule state not saved: ${err instanceof Error ? err.message : String(err)}`);
        }
    }
}
