/**
 * The guided setup: with a token the operator pastes, it finds the account,
 * checks the relay Worker's name is free (or already a Bookclerk relay) and
 * mints an API token that can deploy Workers in that account and do
 * nothing else. The plugin keeps that token to deploy and update the
 * relay; the pasted one is used for this run only.
 *
 * A second run mints a fresh token and deletes the one the first run
 * minted, whose id it keeps in `api_token_id`. Removing the plugin deletes
 * the relay Worker (with the stored token) and the token it minted (which
 * needs a pasted token, since the stored one cannot delete tokens).
 */

import {
  PluginError,
  provisioned,
  type DeprovisionRequest,
  type ProvisionGrant,
  type Provisioned,
  type ProvisionRequest,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { answer } from "./api.ts";
import { isDnsLabel, parseScriptState, type Fetch, type ScriptState } from "./cloudflare.ts";
import {
  SetupError,
  deleteToken,
  findAccount,
  isId,
  mintToken,
  resultOf,
  type Call,
} from "./tokens.ts";

/** The permission groups the minted token gets, by name. */
export const TOKEN_GROUPS = ["Workers Scripts Write"] as const;

/** The Worker name when the setting is unset. */
const DEFAULT_WORKER = "bookclerk-relay";

/** What the setup and its removal reach. */
export interface SetupContext {
  /** The plugin's bindings; `CONFIG` holds the operator's choices. */
  env: Env;
  /** Makes the requests. */
  fetch: Fetch;
}

/**
 * The token's name in the dashboard.
 *
 * @param workerName - The relay Worker's name.
 * @returns `Bookclerk cloudflare_relay <worker>`.
 */
export function tokenName(workerName: string): string {
  return `Bookclerk cloudflare_relay ${workerName}`;
}

/**
 * The resource key of one account, for a token's policy.
 *
 * @param account - The account's id.
 * @returns `com.cloudflare.api.account.<account>`.
 */
export function accountResource(account: string): string {
  return `com.cloudflare.api.account.${account}`;
}

/**
 * The operator's token, when the grant carries one.
 *
 * @param grant - The grant.
 * @returns The trimmed token, or `null` for kind `none` or an empty token.
 */
export function grantToken(grant: ProvisionGrant): string | null {
  if (grant.kind === "none") return null;
  const token = (grant.token ?? "").trim();
  return token === "" ? null : token;
}

/**
 * Turns a setup failure into the plugin error the host shows.
 *
 * @param err - What was thrown.
 * @returns The plugin error; anything else is thrown again.
 * @throws {unknown} `err` when it is not a setup error.
 */
function toPluginError(err: unknown): PluginError {
  if (err instanceof SetupError) return PluginError.fromWire(err.code, err.message);
  throw err;
}

/**
 * The relay Worker's name from the settings.
 *
 * @param env - The bindings.
 * @returns The name.
 * @throws {SetupError} `invalid_params` when it is not a DNS label.
 */
function workerNameOf(env: Env): string {
  const name = String(env.CONFIG.worker_name ?? "").trim() || DEFAULT_WORKER;
  if (!isDnsLabel(name)) {
    throw new SetupError("worker_name must be a lowercase DNS label", "invalid_params");
  }
  return name;
}

/**
 * What the account holds under the relay's name.
 *
 * @param call - Calls the API.
 * @param account - The account's id.
 * @param name - The Worker's name.
 * @returns Its state.
 * @throws {SetupError} When the list cannot be read.
 */
async function scriptState(call: Call, account: string, name: string): Promise<ScriptState> {
  const listed = await call(`/accounts/${account}/workers/scripts`);
  return parseScriptState(resultOf("list the account's Workers", listed), name);
}

/**
 * Runs the guided setup.
 *
 * @param ctx - Bindings and request function.
 * @param request - The operator's grant.
 * @returns The account, the minted token and its id.
 * @throws {PluginError} `invalid_params` for a missing grant or bad
 *   choices, `forbidden` when the token lacks a permission, `unavailable`
 *   when Cloudflare refuses.
 */
export async function provisionRelay(
  ctx: SetupContext,
  request: ProvisionRequest,
): Promise<Provisioned> {
  const token = grantToken(request.grant);
  if (token === null) {
    throw PluginError.fromWire("invalid_params", "the guided setup needs a Cloudflare API token");
  }
  const call: Call = (path, init) => answer(ctx.fetch, token, path, init);
  try {
    const workerName = workerNameOf(ctx.env);
    const account = await findAccount(call, String(ctx.env.CONFIG.account_id ?? "").trim());
    const state = await scriptState(call, account, workerName);
    if (state === "other") {
      throw new SetupError(
        `A Worker named "${workerName}" already exists and is not a Bookclerk relay. Choose another worker_name.`,
        "invalid_params",
      );
    }
    const minted = await mintToken(
      call,
      account,
      tokenName(workerName),
      TOKEN_GROUPS,
      accountResource(account),
    );
    const summary = [
      state === "relay"
        ? `Found your relay Worker ${workerName}`
        : `The relay Worker will be named ${workerName}`,
      "Created an API token that can deploy Workers in this account only",
    ];
    const previous = String(ctx.env.CONFIG.api_token_id ?? "").trim();
    if (isId(previous) && previous !== minted.id) {
      try {
        if ((await deleteToken(call, account, previous)) === "deleted") {
          summary.push("Deleted the token an earlier setup created");
        }
      } catch (err) {
        if (!(err instanceof SetupError)) throw err;
        summary.push(
          `Could not delete the earlier token ${previous}; delete it in the Cloudflare dashboard`,
        );
      }
    }
    summary.push("The relay is deployed when the ingress starts");
    return provisioned({
      settings: { account_id: account, api_token_id: minted.id },
      secrets: { api_token: minted.value },
      summary,
    });
  } catch (err) {
    throw toPluginError(err);
  }
}

/**
 * Removes what the setup and the plugin created: the relay Worker, then
 * the minted token.
 *
 * @param ctx - Bindings and request function.
 * @param request - The operator's grant, or kind `none`.
 * @returns Resolves once both are gone.
 * @throws {PluginError} `unauthorized` when the minted token must be deleted
 *   and no token was pasted, `forbidden` or `unavailable` when Cloudflare
 *   refuses.
 */
export async function deprovisionRelay(
  ctx: SetupContext,
  request: DeprovisionRequest,
): Promise<void> {
  const pasted = grantToken(request.grant);
  const stored = String(ctx.env.SECRETS["api_token"] ?? "").trim();
  const minted = String(ctx.env.CONFIG.api_token_id ?? "").trim();
  if (minted !== "" && pasted === null) {
    throw PluginError.fromWire(
      "unauthorized",
      "deleting the API token the setup created needs a Cloudflare API token with Account: Account API Tokens: Edit",
    );
  }
  const token = pasted ?? (stored === "" ? null : stored);
  if (token === null) return;
  const call: Call = (path, init) => answer(ctx.fetch, token, path, init);
  try {
    const given = String(ctx.env.CONFIG.account_id ?? "").trim();
    const account = isId(given) ? given : await findAccount(call, "");
    const workerName = workerNameOf(ctx.env);
    if ((await scriptState(call, account, workerName)) === "relay") {
      const deleted = await call(`/accounts/${account}/workers/scripts/${workerName}`, {
        method: "DELETE",
      });
      if (!deleted.ok && deleted.status !== 404) resultOf("delete the relay Worker", deleted);
    }
    if (minted !== "") await deleteToken(call, account, minted);
  } catch (err) {
    throw toPluginError(err);
  }
}
