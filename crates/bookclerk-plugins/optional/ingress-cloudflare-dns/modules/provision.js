/**
 * The guided setup: with a token the operator pastes, it finds the host
 * name's zone and mints an API token that can read that zone and edit its
 * DNS records, and nothing else. The plugin keeps that token; the pasted one
 * is used for this run only.
 *
 * A second run mints a fresh token and deletes the one the first run
 * minted, whose id it keeps in `api_token_id`, so a re-run rotates the
 * credential rather than piling up tokens. Removing the plugin deletes the
 * records it kept (with the stored token) and the token it minted (which
 * needs a pasted token, since the stored one cannot delete tokens).
 */
import { PluginError, provisioned, } from "@bookclerk/plugin-sdk/workerd";
import { parseHostname, zoneCandidates } from "./address.js";
import { CloudflareDns, DnsError, RECORD_COMMENT } from "./dns.js";
import { SetupError, deleteToken, findAccount, isId, mintToken } from "./tokens.js";
/** What a 401 or 403 means during the setup. */
export const SETUP_HINT = "The pasted token needs Zone: Zone: Read and Zone: DNS: Edit on the host name's zone, and Account: Account API Tokens: Edit.";
/** The permission groups the minted token gets, by name. */
export const TOKEN_GROUPS = ["Zone Read", "DNS Write"];
/**
 * The token's name in the dashboard.
 *
 * @param hostname - The host name it keeps.
 * @returns `Bookclerk cloudflare_dns <hostname>`.
 */
export function tokenName(hostname) {
    return `Bookclerk cloudflare_dns ${hostname}`;
}
/**
 * The resource key of one zone, for a token's policy.
 *
 * @param zone - The zone's id.
 * @returns `com.cloudflare.api.account.zone.<zone>`.
 */
export function zoneResource(zone) {
    return `com.cloudflare.api.account.zone.${zone}`;
}
/**
 * The operator's token, when the grant carries one.
 *
 * @param grant - The grant.
 * @returns The trimmed token, or `null` for kind `none` or an empty token.
 */
export function grantToken(grant) {
    if (grant.kind === "none")
        return null;
    const token = (grant.token ?? "").trim();
    return token === "" ? null : token;
}
/**
 * Turns a setup failure into the plugin error the host shows.
 *
 * @param err - What was thrown.
 * @returns The plugin error; anything else is thrown again.
 * @throws {unknown} `err` when it is neither a setup nor a DNS error.
 */
function toPluginError(err) {
    if (err instanceof SetupError)
        return PluginError.fromWire(err.code, err.message);
    if (err instanceof DnsError)
        return PluginError.fromWire("unavailable", err.message);
    throw err;
}
/**
 * The zone a host name is in, with the account that owns it.
 *
 * @param dns - The client, with the operator's token.
 * @param hostname - The host name.
 * @returns The zone, its account known.
 * @throws {SetupError} When Cloudflare does not say which account owns it.
 * @throws {DnsError} When no zone holds the name.
 */
async function zoneOf(dns, hostname) {
    const zone = await dns.zone(zoneCandidates(hostname));
    if (zone.account === null) {
        throw new SetupError(`Cloudflare did not say which account owns the zone of ${hostname}`);
    }
    return { id: zone.id, account: zone.account };
}
/**
 * Runs the guided setup.
 *
 * @param ctx - Bindings and request function; `CONFIG` holds the operator's
 *   choices over the stored settings.
 * @param request - The operator's grant.
 * @returns The host name, the minted token and its id.
 * @throws {PluginError} `invalid_params` for a missing grant or bad
 *   choices, `forbidden` when the token lacks a permission, `unavailable`
 *   when Cloudflare refuses.
 */
export async function provisionDns(ctx, request) {
    const token = grantToken(request.grant);
    if (token === null) {
        throw PluginError.fromWire("invalid_params", "the guided setup needs a Cloudflare API token");
    }
    const hostname = parseHostname(String(ctx.env.CONFIG.hostname ?? ""));
    if (hostname === null) {
        throw PluginError.fromWire("invalid_params", "choose a host name in a zone on your Cloudflare account, such as books.example.com");
    }
    if (ctx.env.CONFIG.ipv4 === false && ctx.env.CONFIG.ipv6 !== true) {
        throw PluginError.fromWire("invalid_params", "turn on ipv4, ipv6 or both");
    }
    const dns = new CloudflareDns(ctx.fetch, token, SETUP_HINT);
    const call = (path, init) => dns.answer(path, init);
    try {
        const zone = await zoneOf(dns, hostname);
        const minted = await mintToken(call, zone.account, tokenName(hostname), TOKEN_GROUPS, zoneResource(zone.id));
        const summary = [
            `Found the zone that holds ${hostname}`,
            "Created an API token that can edit DNS in that zone only",
        ];
        const previous = String(ctx.env.CONFIG.api_token_id ?? "").trim();
        if (isId(previous) && previous !== minted.id) {
            try {
                if ((await deleteToken(call, zone.account, previous)) === "deleted") {
                    summary.push("Deleted the token an earlier setup created");
                }
            }
            catch (err) {
                if (!(err instanceof SetupError))
                    throw err;
                summary.push(`Could not delete the earlier token ${previous}; delete it in the Cloudflare dashboard`);
            }
        }
        summary.push("The records are written when the ingress starts");
        return provisioned({
            settings: { hostname, api_token_id: minted.id },
            secrets: { api_token: minted.value },
            summary,
        });
    }
    catch (err) {
        throw toPluginError(err);
    }
}
/**
 * Removes what the setup and the plugin created: the records carrying the
 * plugin's comment, then the minted token.
 *
 * @param ctx - Bindings and request function.
 * @param request - The operator's grant, or kind `none`.
 * @returns Resolves once both are gone.
 * @throws {PluginError} `unauthorized` when the minted token must be deleted
 *   and no token was pasted, `forbidden` or `unavailable` when Cloudflare
 *   refuses.
 */
export async function deprovisionDns(ctx, request) {
    const pasted = grantToken(request.grant);
    const stored = String(ctx.env.SECRETS["api_token"] ?? "").trim();
    const minted = String(ctx.env.CONFIG.api_token_id ?? "").trim();
    const hostname = parseHostname(String(ctx.env.CONFIG.hostname ?? ""));
    const token = pasted ?? (stored === "" ? null : stored);
    if (minted !== "" && pasted === null) {
        throw PluginError.fromWire("unauthorized", "deleting the API token the setup created needs a Cloudflare API token with Account: Account API Tokens: Edit");
    }
    if (token === null)
        return;
    const dns = new CloudflareDns(ctx.fetch, token, pasted === null ? undefined : SETUP_HINT);
    const call = (path, init) => dns.answer(path, init);
    try {
        if (hostname === null) {
            // No records to find; the token can still go.
            if (minted !== "")
                await deleteToken(call, await findAccount(call, ""), minted);
            return;
        }
        const zone = await dns.zone(zoneCandidates(hostname));
        for (const record of await dns.records(zone.id, hostname)) {
            if ((record.type === "A" || record.type === "AAAA") && record.comment === RECORD_COMMENT) {
                await dns.remove(zone.id, record.id);
            }
        }
        if (minted === "")
            return;
        if (zone.account === null) {
            throw new SetupError(`Cloudflare did not say which account owns the zone of ${hostname}`);
        }
        await deleteToken(call, zone.account, minted);
    }
    catch (err) {
        throw toPluginError(err);
    }
}
