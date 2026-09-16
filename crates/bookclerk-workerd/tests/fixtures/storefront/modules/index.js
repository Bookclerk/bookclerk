/**
 * Contract fixture: `Storefront` + `Cli` named entrypoints over `/invoke`.
 *
 * Every method the host calls travels as the typed `$Params` / `$Results`
 * Cap'n envelope; the fixture returns plain author objects (partial structs,
 * `null` for optional results, thrown `PluginError`s) so the contract test
 * can pin how each shape lands on the Rust side.
 */

import {
  BookclerkEntrypoint,
  CliEntrypoint,
  PluginError,
  StorefrontEntrypoint,
} from "@bookclerk/plugin-sdk/workerd";

export class Storefront extends StorefrontEntrypoint {
  async health() {
    return { ok: true, detail: `env=${Object.keys(this.env || {}).sort().join(",")}` };
  }

  async diagnose() {
    return ["line one", "line two"];
  }

  async listAccounts() {
    return [
      { accountId: "acct-1", source: "storefront_fixture", marketplace: "us", label: "One", scanEnabled: true },
      { accountId: "acct-2", source: "storefront_fixture", marketplace: "uk" },
    ];
  }

  async searchCatalog(params) {
    return [
      { productId: `hit:${params.query}:${params.limit}:${params.page}`, title: params.query, authors: "A. Author" },
    ];
  }

  async purchaseHint() {
    return null;
  }

  async catalogDetail(params) {
    if (params.productId === "missing") return null;
    return { productId: params.productId, title: `Detail ${params.productId}` };
  }

  async login(params) {
    throw PluginError.fromWire("unauthorized", `no credentials for ${params.marketplace}`);
  }
}

export class Cli extends CliEntrypoint {
  async describe() {
    return { commands: [{ name: "echo", about: "Echo the arguments", args: [] }] };
  }

  async invoke(params) {
    return {
      exitCode: 3,
      stdout: `${params.command}:${(params.args || []).map((a) => `${a.name}=${a.value}`).join(";")}`,
      stderr: "",
    };
  }
}

export default class StorefrontPlugin extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Storefront contract fixture" };
  }
}
