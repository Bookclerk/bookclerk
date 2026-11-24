/**
 * The guided setup: with a token the operator pastes, it creates (or
 * finds) a remotely managed tunnel, routes the host name to it and points
 * the host name's DNS at it, then keeps only the tunnel's own token, which
 * can run the tunnel and do nothing else. The pasted token is used for this
 * run only.
 *
 * A second run finds the tunnel again from the stored token (or by name)
 * rather than creating another. Removing the plugin deletes the DNS record
 * and the tunnel; the tunnel token cannot call the API, so that needs a
 * pasted token.
 */
import { PluginError, provisioned, } from "@bookclerk/plugin-sdk/workerd";
import { SetupError, answer, findAccount, parseRecords, parseTunnel, parseTunnels, parseTunnelToken, parseZone, resultOf, zoneCandidates, } from "./cfapi.js";
import { hostnameOk, parseToken } from "./lifecycle.js";
/** The comment on the DNS record the setup keeps. */
export const RECORD_COMMENT = "Kept by Bookclerk (cloudflare_tunnel)";
/**
 * The tunnel's name in the dashboard.
 *
 * @param hostname - The host name it serves.
 * @returns `bookclerk-<hostname>`.
 */
export function tunnelName(hostname) {
    return `bookclerk-${hostname}`;
}
/**
 * The name a tunnel's DNS record points at.
 *
 * @param tunnel - The tunnel's id.
 * @returns `<tunnel>.cfargotunnel.com`.
 */
export function tunnelTarget(tunnel) {
    return `${tunnel}.cfargotunnel.com`;
}
/**
 * The tunnel's routing: the host name to the library (the plugin serves
 * every request it is handed, so the service is only a placeholder), and
 * nothing else.
 *
 * @param hostname - The host name.
 * @returns The configuration body.
 */
export function tunnelConfig(hostname) {
    return {
        config: {
            ingress: [{ hostname, service: "http://localhost:8787" }, { service: "http_status:404" }],
        },
    };
}
/**
 * A tunnel id from its 16 bytes.
 *
 * @param bytes - The id.
 * @returns The lowercase UUID.
 */
export function uuidOf(bytes) {
    const hex = [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
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
 * The tunnel the stored token runs, when it decodes.
 *
 * @param env - The bindings.
 * @returns The account and tunnel ids, or `null`.
 */
function storedTunnel(env) {
    const raw = String(env.SECRETS["token"] ?? "").trim();
    if (raw === "")
        return null;
    const token = parseToken(raw);
    if (typeof token === "string")
        return null;
    return { account: token.accountTag, tunnel: uuidOf(token.tunnelId) };
}
/**
 * The host name from the settings.
 *
 * @param env - The bindings.
 * @returns The host name, lowercase, or `null` when it is not one.
 */
function hostnameOf(env) {
    const hostname = String(env.CONFIG.hostname ?? "")
        .trim()
        .toLowerCase()
        .replace(/^https:\/\//, "")
        .replace(/\/$/, "")
        .replace(/\.$/, "");
    return hostnameOk(hostname) ? hostname : null;
}
/**
 * Turns a setup failure into the plugin error the host shows.
 *
 * @param err - What was thrown.
 * @returns The plugin error; anything else is thrown again.
 * @throws {unknown} `err` when it is not a setup error.
 */
function toPluginError(err) {
    if (err instanceof SetupError)
        return PluginError.fromWire(err.code, err.message);
    throw err;
}
/**
 * The zone that holds a host name.
 *
 * @param call - Calls the API.
 * @param hostname - The host name.
 * @returns The zone's id.
 * @throws {SetupError} When no candidate is an active zone the token reaches.
 */
async function zoneOf(call, hostname) {
    for (const name of zoneCandidates(hostname)) {
        const listed = await call(`/zones?name=${encodeURIComponent(name)}&status=active`);
        const zone = parseZone(resultOf("find the host name's zone", listed), name);
        if (zone !== null)
            return zone;
    }
    throw new SetupError(`no active Cloudflare zone holds ${hostname}: add its domain to Cloudflare, or give the token access to its zone`, "invalid_params");
}
/**
 * Finds the tunnel an earlier run made, by id or by name, or creates one.
 *
 * @param call - Calls the API.
 * @param account - The account's id.
 * @param hostname - The host name.
 * @param known - The tunnel id the stored token names, if any.
 * @returns The tunnel, and whether it was created now.
 * @throws {SetupError} When Cloudflare refuses.
 */
async function tunnelFor(call, account, hostname, known) {
    const base = `/accounts/${account}/cfd_tunnel`;
    if (known !== null) {
        const found = await call(`${base}/${known}`);
        const tunnel = found.ok ? parseTunnel(found.result) : null;
        if (tunnel !== null)
            return { tunnel, created: false };
    }
    const name = tunnelName(hostname);
    const listed = await call(`${base}?name=${encodeURIComponent(name)}&is_deleted=false`);
    const named = parseTunnels(resultOf("list the account's tunnels", listed), name);
    if (named !== null)
        return { tunnel: named, created: false };
    const made = await call(base, { method: "POST", body: { name, config_src: "cloudflare" } });
    const tunnel = parseTunnel(resultOf("create the tunnel", made));
    if (tunnel === null)
        throw new SetupError("Cloudflare's new tunnel has no usable id");
    return { tunnel, created: true };
}
/**
 * Points the host name's DNS at the tunnel: a proxied CNAME.
 *
 * @param call - Calls the API.
 * @param zone - The zone's id.
 * @param hostname - The host name.
 * @param tunnel - The tunnel's id.
 * @returns What it did, for the summary.
 * @throws {SetupError} When the name holds other records, or Cloudflare
 *   refuses.
 */
async function pointDns(call, zone, hostname, tunnel) {
    const base = `/zones/${zone}/dns_records`;
    const listed = await call(`${base}?name=${encodeURIComponent(hostname)}&per_page=100`);
    const records = parseRecords(resultOf("list the host name's records", listed), hostname);
    const target = tunnelTarget(tunnel);
    const body = {
        type: "CNAME",
        name: hostname,
        content: target,
        proxied: true,
        comment: RECORD_COMMENT,
    };
    const others = records.filter((r) => r.type !== "CNAME");
    if (others.length > 0) {
        throw new SetupError(`${hostname} already has ${others[0]?.type} records; delete them in the Cloudflare dashboard or choose another host name`, "invalid_params");
    }
    const [cname] = records;
    if (cname === undefined) {
        resultOf("create the DNS record", await call(base, { method: "POST", body }));
        return `Pointed ${hostname} at the tunnel`;
    }
    if (cname.content === target && cname.proxied && cname.comment === RECORD_COMMENT) {
        return `${hostname} already points at the tunnel`;
    }
    const ours = cname.comment === RECORD_COMMENT || cname.content.endsWith(".cfargotunnel.com");
    if (!ours) {
        throw new SetupError(`${hostname} already points at ${cname.content}; delete that record in the Cloudflare dashboard or choose another host name`, "invalid_params");
    }
    resultOf("update the DNS record", await call(`${base}/${cname.id}`, { method: "PATCH", body }));
    return `Pointed ${hostname} at the tunnel`;
}
/**
 * Runs the guided setup.
 *
 * @param ctx - Bindings and request function.
 * @param request - The operator's grant.
 * @returns The account, the host name and the tunnel token.
 * @throws {PluginError} `invalid_params` for a missing grant or bad
 *   choices, `forbidden` when the token lacks a permission, `unavailable`
 *   when Cloudflare refuses.
 */
export async function provisionTunnel(ctx, request) {
    const token = grantToken(request.grant);
    if (token === null) {
        throw PluginError.fromWire("invalid_params", "the guided setup needs a Cloudflare API token");
    }
    const hostname = hostnameOf(ctx.env);
    if (hostname === null) {
        throw PluginError.fromWire("invalid_params", "choose a host name in a zone on your Cloudflare account, such as books.example.com");
    }
    const call = (path, init) => answer(ctx.fetch, token, path, init);
    try {
        const stored = storedTunnel(ctx.env);
        const given = String(ctx.env.CONFIG.account_id ?? "").trim();
        const account = await findAccount(call, given !== "" ? given : (stored?.account ?? ""));
        const zone = await zoneOf(call, hostname);
        const known = stored !== null && stored.account === account ? stored.tunnel : null;
        const { tunnel, created } = await tunnelFor(call, account, hostname, known);
        resultOf("route the host name to the tunnel", await call(`/accounts/${account}/cfd_tunnel/${tunnel.id}/configurations`, {
            method: "PUT",
            body: tunnelConfig(hostname),
        }));
        const dns = await pointDns(call, zone, hostname, tunnel.id);
        const fetched = await call(`/accounts/${account}/cfd_tunnel/${tunnel.id}/token`);
        const tunnelToken = parseTunnelToken(resultOf("read the tunnel's token", fetched));
        return provisioned({
            settings: { account_id: account, hostname },
            secrets: { token: tunnelToken },
            summary: [
                created ? `Created the tunnel ${tunnel.name}` : `Found the tunnel ${tunnel.name}`,
                `Routed ${hostname} to it`,
                dns,
                "Kept the tunnel's own token, which can run this tunnel only",
            ],
        });
    }
    catch (err) {
        throw toPluginError(err);
    }
}
/**
 * Removes what the setup created: the DNS record, then the tunnel.
 *
 * @param ctx - Bindings and request function.
 * @param request - The operator's grant, or kind `none`.
 * @returns Resolves once both are gone.
 * @throws {PluginError} `unauthorized` when there is a tunnel to delete and
 *   no token was pasted, `forbidden` or `unavailable` when Cloudflare
 *   refuses.
 */
export async function deprovisionTunnel(ctx, request) {
    const stored = storedTunnel(ctx.env);
    if (stored === null)
        return;
    const token = grantToken(request.grant);
    if (token === null) {
        throw PluginError.fromWire("unauthorized", "deleting the tunnel needs a Cloudflare API token with Account: Cloudflare Tunnel: Edit, Zone: Zone: Read and Zone: DNS: Edit");
    }
    const call = (path, init) => answer(ctx.fetch, token, path, init);
    try {
        const hostname = hostnameOf(ctx.env);
        if (hostname !== null) {
            const zone = await zoneOf(call, hostname);
            const base = `/zones/${zone}/dns_records`;
            const listed = await call(`${base}?name=${encodeURIComponent(hostname)}&per_page=100`);
            for (const record of parseRecords(resultOf("list the host name's records", listed), hostname)) {
                if (record.type === "CNAME" && record.content === tunnelTarget(stored.tunnel)) {
                    const gone = await call(`${base}/${record.id}`, { method: "DELETE" });
                    if (!gone.ok && gone.status !== 404)
                        resultOf("delete the DNS record", gone);
                }
            }
        }
        const tunnel = `/accounts/${stored.account}/cfd_tunnel/${stored.tunnel}`;
        // Drop the tunnel's connections first: Cloudflare refuses to delete a
        // tunnel that still has some.
        const dropped = await call(`${tunnel}/connections`, { method: "DELETE" });
        if (!dropped.ok && dropped.status !== 404)
            resultOf("drop the tunnel's connections", dropped);
        const deleted = await call(tunnel, { method: "DELETE" });
        if (!deleted.ok && deleted.status !== 404)
            resultOf("delete the tunnel", deleted);
    }
    catch (err) {
        throw toPluginError(err);
    }
}
