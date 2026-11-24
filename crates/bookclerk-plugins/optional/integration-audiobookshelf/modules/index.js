/**
 * Audiobookshelf integration (workerd guest).
 *
 * Exports the `remoteLibrary`, `oidc` and `metadataFiles` entrypoints
 * `plugin.toml` declares, plus the default entrypoint's `book_acquired` consumer, the
 * `[sockets]` handler and the database migrations. The work happens in
 * `guest.ts`; this module reads the bindings and turns typed failures into
 * `PluginError`s. The sidecar formats (`metadata.json`, `.nfo`,
 * `desc.txt`, `reader.txt`) live behind `dialects.ts`.
 */
import { BookclerkEntrypoint, MetadataFilesEntrypoint, OidcEntrypoint, PluginError, RemoteLibraryEntrypoint, } from "@bookclerk/plugin-sdk/workerd";
import { AbsFailure } from "./client.js";
import { parseFile, renderFile } from "./dialects.js";
import * as guest from "./guest.js";
import { migrationsFor } from "./listening.js";
import { PROVIDER, readSettings } from "./settings.js";
import { DialectFailure } from "./text.js";
/** Brand shown on connect-portal buttons. */
export const BRAND = {
    id: PROVIDER,
    name: "Audiobookshelf",
    bg: "#1E293B",
    fg: "#F8FAFC",
    accent: "#59BC89",
    iconUrl: "https://www.google.com/s2/favicons?domain=audiobookshelf.org&sz=128",
};
/** The relying-party template Bookclerk offers Audiobookshelf as an OpenID Connect provider. */
export const OIDC_CLIENT = {
    clientId: PROVIDER,
    displayName: "Audiobookshelf",
    callbackPath: "/auth/openid/callback",
    publicClient: true,
    defaultScopes: ["openid", "profile"],
    issueRefreshToken: true,
    originConfigKey: "integrations.audiobookshelf.base_url",
};
/** What the isolate keeps between calls. */
const STATE = guest.newState();
/**
 * Runs `call`, turning an `AbsFailure` into a `PluginError` with its code.
 *
 * @param call - The work.
 * @returns What `call` returns.
 * @throws {PluginError} When `call` fails.
 */
export async function guarded(call) {
    try {
        return await call();
    }
    catch (err) {
        if (err instanceof AbsFailure)
            throw PluginError.fromWire(err.code, err.message);
        throw err;
    }
}
/**
 * Runs a dialect call, turning its refusal into a `PluginError`.
 *
 * @param work - The call.
 * @returns What the call returned.
 * @throws {PluginError} With the refusal's code.
 */
export function dialectGuarded(work) {
    try {
        return Promise.resolve(work());
    }
    catch (err) {
        if (err instanceof DialectFailure) {
            return Promise.reject(PluginError.fromWire(err.code, err.message));
        }
        return Promise.reject(err);
    }
}
/**
 * The call context for an entrypoint's bindings.
 *
 * @param env - The entrypoint's `env`.
 * @returns Settings, `fetch` and the granted bindings.
 */
export function contextOf(env) {
    return {
        settings: readSettings(env.CONFIG, env.SECRETS),
        fetch: (url, init) => fetch(url, init),
        db: env.LISTENING_STATE,
        sockets: env.SOCKETS,
    };
}
/** `remoteLibrary` entrypoint: health, rescans, listening sync and the user watch. */
export class RemoteLibrary extends RemoteLibraryEntrypoint {
    /**
     * Checks the API key; a misconfiguration reports not ok.
     *
     * @returns Health and detail.
     */
    health() {
        return guest.health(contextOf(this.env));
    }
    /**
     * Lists who the key belongs to and the libraries it sees.
     *
     * @returns Diagnostic lines.
     */
    diagnose() {
        return guarded(() => guest.diagnose(contextOf(this.env)));
    }
    /**
     * Seeds the user watch and opens the pushed-event socket.
     *
     * @returns Resolves when started.
     */
    start() {
        return guarded(() => guest.start(STATE, contextOf(this.env)));
    }
    /**
     * Asks Audiobookshelf to rescan the configured library.
     *
     * @param params - Whether to force the rescan.
     * @returns Resolves once Audiobookshelf accepted the scan.
     */
    scanLibrary(params) {
        return guarded(() => guest.scanLibrary(contextOf(this.env), params.force === true));
    }
    /**
     * The listening-progress rows that changed since the last sync.
     *
     * @returns Rows for the host to upsert.
     */
    syncListening() {
        return guarded(() => guest.syncListening(STATE, contextOf(this.env), Date.now()));
    }
    /**
     * The users created since the last poll, plus those the host has not
     * acknowledged yet when it acknowledges polls.
     *
     * @param params - Whether an `ackEvents` call follows (absent from older hosts).
     * @returns New users, for claim tickets.
     */
    pollEvents(params) {
        const ackFollows = params?.ackFollows === true;
        return guarded(() => guest.pollEvents(STATE, contextOf(this.env), ackFollows));
    }
    /**
     * Records that the host received these users, so polls stop reporting them.
     *
     * @param params - The acknowledged Audiobookshelf user ids.
     */
    ackEvents(params) {
        return guarded(() => guest.ackEvents(STATE, contextOf(this.env), params.externalUserIds));
    }
}
/** `oidc` entrypoint: the relying-party template and credential sign-in. */
export class Oidc extends OidcEntrypoint {
    /**
     * The relying-party templates this plugin provides.
     *
     * @returns The Audiobookshelf client.
     */
    clients() {
        return Promise.resolve([OIDC_CLIENT]);
    }
    /**
     * Signs a person in with their Audiobookshelf credentials.
     *
     * @param params - Login name and password.
     * @returns The signed-in user.
     */
    authenticateUser(params) {
        return guarded(() => guest.authenticateUser(contextOf(this.env), params.username, params.password));
    }
}
/**
 * `metadataFiles` entrypoint: reads and writes Audiobookshelf's sidecar
 * files. The host hands every file in and takes every file back, so this
 * needs no binding.
 */
export class MetadataFiles extends MetadataFilesEntrypoint {
    /**
     * Reports ready; the formats need nothing outside the request.
     *
     * @returns Always ready.
     */
    health() {
        return Promise.resolve({ ok: true, detail: "" });
    }
    /**
     * Reads one file.
     *
     * @param params - Dialect id, file name and the whole file.
     * @returns What the file states.
     * @throws {PluginError} `invalid_params` for an unknown dialect or a file
     *   the dialect cannot read.
     */
    parse(params) {
        return dialectGuarded(() => parseFile(params.dialect, params.bytes));
    }
    /**
     * Writes one file.
     *
     * @param params - Dialect id, values, fields to set and the current file.
     * @returns The new file's bytes.
     * @throws {PluginError} `invalid_params` for an unknown dialect, an
     *   existing file the dialect cannot read, or a result over the size cap.
     */
    render(params) {
        return dialectGuarded(() => renderFile(params.dialect, params.existing, params.record, params.fields));
    }
}
/** Default entrypoint: identity, the `book_acquired` consumer, the socket handler and migrations. */
export default class AudiobookshelfPlugin extends BookclerkEntrypoint {
    /**
     * Describes the plugin.
     *
     * @returns Display name, the `abs` alias and the portal brand.
     */
    async describe() {
        return { displayName: "Audiobookshelf", aliases: ["abs"], brand: { ...BRAND } };
    }
    /**
     * Asks Audiobookshelf to rescan after each `book_acquired`.
     *
     * @param batch - Delivered events.
     */
    async event(batch) {
        const ctx = contextOf(this.env);
        for (const message of batch.messages) {
            const decision = await guest.onEvent(ctx, message.type);
            if (decision.kind === "ack")
                message.ack();
            else
                message.retry({ reason: decision.reason });
        }
    }
    /**
     * Answers a frame pushed on the Audiobookshelf socket.
     *
     * @param ws - The socket.
     * @param message - The frame.
     * @returns `pollEvents: true` when the server announced a new user.
     */
    async webSocketMessage(ws, message) {
        return { pollEvents: await guest.pushFrame(contextOf(this.env), ws, message) };
    }
    /**
     * The migrations of `LISTENING_STATE`.
     *
     * @param binding - Binding name from `plugin.toml`.
     * @returns The binding's migrations, oldest first.
     */
    databaseMigrations(binding) {
        return migrationsFor(binding);
    }
}
