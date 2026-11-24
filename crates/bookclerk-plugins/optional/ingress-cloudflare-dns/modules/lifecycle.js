/**
 * The dynamic DNS ingress's lifecycle, apart from the entrypoint classes.
 *
 * Each `start` and `health` learns this computer's public addresses from
 * the address echo, one request per enabled family, and points the host
 * name's A and AAAA records at them. While the addresses stay the same the
 * records are checked again only every {@link RECHECK_MS}, so a minute's
 * health check costs two small requests and no API call.
 *
 * workerd treats every named export of a Worker's main module as an
 * entrypoint, so `index.ts` exports only the classes and this module holds
 * the logic they call, which the tests drive with fakes.
 */
import { PluginError, } from "@bookclerk/plugin-sdk/workerd";
import { MAX_ECHO_BYTES, isPublicAddress, parseEchoAnswer, parseHostname, zoneCandidates, } from "./address.js";
import { CloudflareDns, DnsError, RECORD_COMMENT, RECORD_TTL, readCapped, } from "./dns.js";
/** How long records known to be current go unchecked, in milliseconds. */
export const RECHECK_MS = 30 * 60 * 1000;
/** How long one echo request may take, in milliseconds. */
export const ECHO_TIMEOUT_MS = 10_000;
/** The address echo for each family: names with only an A or only an AAAA record. */
export const ECHO_URLS = {
    4: "https://ipv4.icanhazip.com/",
    6: "https://ipv6.icanhazip.com/",
};
/** The record type for each family. */
const RECORD_TYPE = { 4: "A", 6: "AAAA" };
/** The records last made current; `null` until a check succeeds. */
let applied = null;
/**
 * The context the entrypoints pass.
 *
 * @param env - The plugin's bindings.
 * @returns A context with the global `fetch` and clock.
 */
export function liveContext(env) {
    return { env, fetch: (input, init) => fetch(input, init), now: () => Date.now() };
}
/**
 * Reads and checks the instance settings.
 *
 * @param env - The plugin's bindings.
 * @returns The settings.
 * @throws {PluginError} `invalid_params` when one is missing or malformed.
 */
export function settingsOf(env) {
    const hostname = parseHostname(String(env.CONFIG.hostname ?? ""));
    if (hostname === null) {
        throw PluginError.fromWire("invalid_params", "set hostname to a host name in a zone on your Cloudflare account, such as books.example.com");
    }
    const families = [];
    if (env.CONFIG.ipv4 !== false)
        families.push(4);
    if (env.CONFIG.ipv6 === true)
        families.push(6);
    if (families.length === 0) {
        throw PluginError.fromWire("invalid_params", "turn on ipv4, ipv6 or both");
    }
    const token = String(env.SECRETS["api_token"] ?? "").trim();
    if (!token) {
        throw PluginError.fromWire("invalid_params", "set the api_token secret to a Cloudflare API token");
    }
    return { hostname, families, token };
}
/**
 * Learns one family's public address from the echo.
 *
 * @param fetch - The request function.
 * @param family - The family.
 * @returns The address, or why there is none.
 */
export async function learn(fetch, family) {
    const name = family === 4 ? "IPv4" : "IPv6";
    let response;
    try {
        response = await fetch(ECHO_URLS[family], {
            redirect: "manual",
            signal: AbortSignal.timeout(ECHO_TIMEOUT_MS),
        });
    }
    catch {
        return { problem: `this computer has no ${name} route to the internet` };
    }
    const body = response.ok ? await readCapped(response, MAX_ECHO_BYTES).catch(() => null) : null;
    if (!response.ok)
        await response.body?.cancel().catch(() => { });
    const address = body === null ? null : parseEchoAnswer(body, family);
    if (address === null)
        return { problem: `the ${name} address echo gave no address` };
    if (!isPublicAddress(address, family)) {
        return { problem: `this computer's ${name} address ${address} is not public` };
    }
    return { address };
}
/**
 * Makes one family's record match its address.
 *
 * @param dns - The API client.
 * @param zone - The zone's id.
 * @param hostname - The host name.
 * @param type - The record type.
 * @param want - The address, or `null` to remove the plugin's record.
 * @param records - The name's records of that type.
 * @returns Resolves once the record matches.
 * @throws {DnsError} When Cloudflare refuses, or the name holds more than
 *   one record of that type.
 */
async function reconcileType(dns, zone, hostname, type, want, records) {
    if (want === null) {
        for (const record of records) {
            if (record.comment === RECORD_COMMENT)
                await dns.remove(zone, record.id);
        }
        return;
    }
    if (records.length > 1) {
        throw new DnsError(`${hostname} has ${records.length} ${type} records; delete all but one in the Cloudflare dashboard`);
    }
    const [record] = records;
    if (record === undefined) {
        await dns.create(zone, hostname, type, want);
        return;
    }
    const current = record.content === want &&
        !record.proxied &&
        record.ttl === RECORD_TTL &&
        record.comment === RECORD_COMMENT;
    if (!current)
        await dns.update(zone, record.id, want);
}
/**
 * Learns the addresses and makes the records match them.
 *
 * @param ctx - Bindings, request function and clock.
 * @param settings - The checked settings.
 * @param force - Whether to check the records even when nothing changed.
 * @returns The families published and the problems with the others.
 * @throws {DnsError} When no address could be learned or Cloudflare
 *   refuses.
 */
export async function reconcile(ctx, settings, force) {
    const addresses = { 4: null, 6: null };
    const problems = [];
    for (const family of settings.families) {
        const learned = await learn(ctx.fetch, family);
        if ("address" in learned)
            addresses[family] = learned.address;
        else
            problems.push(learned.problem);
    }
    const published = settings.families.filter((family) => addresses[family] !== null);
    if (published.length === 0) {
        throw new DnsError(`found no public address to publish: ${problems.join("; ")}`);
    }
    const unchanged = applied !== null &&
        applied.hostname === settings.hostname &&
        published.every((family) => applied?.addresses[family] === addresses[family]) &&
        ctx.now() - applied.checkedAt < RECHECK_MS;
    if (unchanged && !force)
        return { published, problems };
    const dns = new CloudflareDns(ctx.fetch, settings.token);
    const zone = await dns.zone(zoneCandidates(settings.hostname));
    const records = await dns.records(zone, settings.hostname);
    if (records.some((record) => record.type === "CNAME")) {
        throw new DnsError(`${settings.hostname} is a CNAME; delete it in the Cloudflare dashboard so the plugin can keep its address records`);
    }
    const kept = { 4: null, 6: null };
    for (const family of [4, 6]) {
        const type = RECORD_TYPE[family];
        const ofType = records.filter((record) => record.type === type);
        if (!settings.families.includes(family)) {
            // Turned off: the plugin takes down only the record it kept.
            await reconcileType(dns, zone, settings.hostname, type, null, ofType);
        }
        else if (addresses[family] !== null) {
            await reconcileType(dns, zone, settings.hostname, type, addresses[family], ofType);
            kept[family] = addresses[family];
        }
        // On but not learned now: the record is left as it is, since the
        // family may be back on the next check.
    }
    applied = { hostname: settings.hostname, addresses: kept, checkedAt: ctx.now() };
    return { published, problems };
}
/**
 * Brings the address up: points the records at this computer.
 *
 * @param ctx - Bindings, request function and clock.
 * @returns The public origin, `https://` and the host name.
 * @throws {PluginError} `invalid_params` for bad settings, `unavailable`
 *   when no address could be published.
 */
export async function startDns(ctx) {
    const settings = settingsOf(ctx.env);
    try {
        await reconcile(ctx, settings, true);
    }
    catch (err) {
        if (err instanceof DnsError)
            throw PluginError.fromWire("unavailable", err.message);
        throw err;
    }
    return { origin: `https://${settings.hostname}` };
}
/**
 * Checks the addresses and follows a change.
 *
 * @param ctx - Bindings, request function and clock.
 * @returns Serving while at least one family's record points here; the
 *   detail names any enabled family that is not published.
 */
export async function dnsHealth(ctx) {
    let settings;
    try {
        settings = settingsOf(ctx.env);
    }
    catch (err) {
        return { serving: false, detail: err instanceof Error ? err.message : String(err) };
    }
    try {
        const outcome = await reconcile(ctx, settings, false);
        return outcome.problems.length === 0
            ? { serving: true }
            : { serving: true, detail: outcome.problems.join("; ") };
    }
    catch (err) {
        if (err instanceof DnsError)
            return { serving: false, detail: err.message };
        throw err;
    }
}
/**
 * Stops following the addresses. The records stay, so starting again
 * serves under the same name at once; the next check corrects them.
 */
export function stopDns() {
    applied = null;
}
