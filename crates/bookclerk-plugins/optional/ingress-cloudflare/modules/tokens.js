/**
 * Account API tokens for the guided setup: finding the account, looking up
 * permission groups, and minting and deleting the narrow token the plugin
 * keeps.
 *
 * The answers are parsed without I/O ({@link parseAccounts},
 * {@link parsePermissionGroup}, {@link parseMintedToken}) so the fuzz
 * target drives the same code.
 */
/** Why a setup step failed, for the operator. */
export class SetupError extends Error {
    /** The plugin error code to answer with. */
    code;
    /**
     * Creates the error.
     *
     * @param message - What went wrong and what to do; never a credential.
     * @param code - The plugin error code to answer with.
     */
    constructor(message, code = "unavailable") {
        super(message);
        this.name = "SetupError";
        this.code = code;
    }
}
/** Cloudflare's ids: 32 lowercase hex digits. */
const ID = /^[0-9a-f]{32}$/;
/** A token value the host can seal: printable ASCII without spaces. */
const TOKEN_VALUE = /^[\x21-\x7e]{1,1024}$/;
/**
 * Whether `value` is one of Cloudflare's ids.
 *
 * @param value - Candidate.
 * @returns `true` for a string of 32 lowercase hex digits.
 */
export function isId(value) {
    return typeof value === "string" && ID.test(value);
}
/**
 * Reads an account list.
 *
 * @param result - The list's `result`.
 * @returns The ids of the entries with a valid id, in order.
 * @throws {SetupError} When the result is not a list.
 */
export function parseAccounts(result) {
    if (!Array.isArray(result))
        throw new SetupError("Cloudflare's account list is not a list");
    const ids = [];
    for (const entry of result) {
        if (typeof entry !== "object" || entry === null)
            continue;
        const id = entry.id;
        if (isId(id))
            ids.push(id);
    }
    return ids;
}
/**
 * Finds a permission group by its exact name.
 *
 * @param result - The permission group list's `result`.
 * @param name - The group's name, such as `DNS Write`.
 * @returns The first such group's id, or `null` when none is listed.
 * @throws {SetupError} When the result is not a list.
 */
export function parsePermissionGroup(result, name) {
    if (!Array.isArray(result)) {
        throw new SetupError("Cloudflare's permission group list is not a list");
    }
    for (const entry of result) {
        if (typeof entry !== "object" || entry === null)
            continue;
        const group = entry;
        if (group.name === name && isId(group.id))
            return group.id;
    }
    return null;
}
/**
 * Reads a token create answer.
 *
 * @param result - The answer's `result`.
 * @returns The new token's id and value.
 * @throws {SetupError} When either is missing or malformed.
 */
export function parseMintedToken(result) {
    if (typeof result !== "object" || result === null) {
        throw new SetupError("Cloudflare's new token is not an object");
    }
    const token = result;
    if (!isId(token.id))
        throw new SetupError("Cloudflare's new token has no usable id");
    if (typeof token.value !== "string" || !TOKEN_VALUE.test(token.value)) {
        throw new SetupError("Cloudflare's new token has no usable value");
    }
    return { id: token.id, value: token.value };
}
/**
 * The body of a token create call: one policy allowing `groups` on one
 * resource.
 *
 * @param name - The token's name in the dashboard.
 * @param groups - Permission group ids.
 * @param resource - The resource key, such as
 *   `com.cloudflare.api.account.zone.<zone id>`.
 * @returns The JSON body.
 */
export function tokenRequest(name, groups, resource) {
    return {
        name,
        policies: [
            {
                effect: "allow",
                permission_groups: groups.map((id) => ({ id })),
                resources: { [resource]: "*" },
            },
        ],
    };
}
/**
 * The text of a failed answer, for the operator.
 *
 * @param step - What the setup was doing, such as `create the token`.
 * @param answer - The failed answer.
 * @returns `could not <step>: Cloudflare answered <status>[: <message>]`.
 */
export function refusal(step, answer) {
    const said = answer.message
        ? `Cloudflare answered ${answer.status}: ${answer.message}`
        : `Cloudflare answered ${answer.status}`;
    return `could not ${step}: ${said}`;
}
/**
 * The plugin error code for a refused call.
 *
 * @param status - The answer's HTTP status.
 * @returns `forbidden` for 401 and 403, else `unavailable`.
 */
export function codeOf(status) {
    return status === 401 || status === 403 ? "forbidden" : "unavailable";
}
/**
 * The result of an answer that must have succeeded.
 *
 * @param step - What the setup was doing.
 * @param answer - The answer.
 * @returns Its `result`.
 * @throws {SetupError} When it failed.
 */
export function resultOf(step, answer) {
    if (!answer.ok)
        throw new SetupError(refusal(step, answer), codeOf(answer.status));
    return answer.result;
}
/**
 * Finds the account the setup works in.
 *
 * @param call - Calls the API with the operator's token.
 * @param given - The account id the operator chose, or `""`.
 * @returns `given` when set, else the only account the token reaches.
 * @throws {SetupError} When `given` is malformed, or the token reaches no
 *   account or more than one.
 */
export async function findAccount(call, given) {
    if (given !== "") {
        if (!isId(given)) {
            throw new SetupError("account_id must be 32 lowercase hex digits", "invalid_params");
        }
        return given;
    }
    const ids = parseAccounts(resultOf("list your accounts", await call("/accounts?per_page=50")));
    if (ids.length === 1)
        return ids[0];
    throw new SetupError(ids.length === 0
        ? "the token reaches no Cloudflare account"
        : "the token reaches more than one account: set account_id", ids.length === 0 ? "forbidden" : "invalid_params");
}
/**
 * Creates an account API token.
 *
 * @param call - Calls the API with the operator's token.
 * @param account - The account's id.
 * @param name - The token's name in the dashboard.
 * @param groupNames - Permission group names, such as `DNS Write`.
 * @param resource - The one resource the token reaches.
 * @returns The new token.
 * @throws {SetupError} When a group is not offered or Cloudflare refuses.
 */
export async function mintToken(call, account, name, groupNames, resource) {
    const groups = [];
    for (const group of groupNames) {
        const answer = await call(`/accounts/${account}/tokens/permission_groups?name=${encodeURIComponent(group)}`);
        const id = parsePermissionGroup(resultOf("read the token permissions", answer), group);
        if (id === null) {
            throw new SetupError(`the token cannot grant "${group}": give it the Account API Tokens: Edit permission`, "forbidden");
        }
        groups.push(id);
    }
    const answer = await call(`/accounts/${account}/tokens`, {
        method: "POST",
        body: tokenRequest(name, groups, resource),
    });
    return parseMintedToken(resultOf("create the plugin's token", answer));
}
/**
 * Deletes an account API token the setup created earlier.
 *
 * @param call - Calls the API with the operator's token.
 * @param account - The account's id.
 * @param id - The token's id.
 * @returns `deleted`, or `gone` when Cloudflare no longer has it.
 * @throws {SetupError} When the id is malformed or Cloudflare refuses.
 */
export async function deleteToken(call, account, id) {
    if (!isId(id))
        throw new SetupError("the stored token id is not 32 lowercase hex digits");
    const answer = await call(`/accounts/${account}/tokens/${id}`, { method: "DELETE" });
    if (answer.ok)
        return "deleted";
    if (answer.status === 404)
        return "gone";
    throw new SetupError(refusal("delete the token an earlier setup created", answer), codeOf(answer.status));
}
