import {
  BookclerkPlugin,
  Integration,
  PRODUCT_API_VERSION,
  FEATURE_SCALAR_LIMITS,
  MAX_LIST_PAGE,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  type DomainEvent,
  type EventResult,
  type PluginDescribe,
} from "@bookclerk/plugin-sdk/workerd";

const PLUGIN_ID = "echo_workerd_fetch";
const EXAMPLE_URL = "https://www.example.com/";

const CLI = {
  commands: [
    {
      name: "ping",
      about: "Probe echo plugin",
      args: [
        {
          name: "message",
          long: "message",
          kind: "string",
          default: "hi",
        },
      ],
    },
    {
      name: "fetch-example",
      about:
        "GET https://www.example.com/ (succeeds if allowlisted; HTTP status ignored)",
      args: [],
    },
  ],
};

/**
 * Probe `www.example.com` under the `*.example.com` consent allowlist.
 *
 * Success means egress allowed the request (a Response was returned). HTTP
 * status is best-effort and must not fail the probe — example.com may return
 * any status or be intermittently unreachable at the origin while still
 * proving the allowlist hop worked.
 */
async function probeExampleFetch(): Promise<{ allowed: boolean; detail: string }> {
  try {
    const res = await fetch(EXAMPLE_URL);
    return {
      allowed: true,
      detail: `example.com fetch allowed (HTTP ${res.status})`,
    };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      allowed: false,
      detail: `example.com fetch denied or failed before response: ${message}`,
    };
  }
}

class EchoFetchIntegration extends Integration {
  constructor(private readonly pluginEnv: BookclerkPlugin["env"]) {
    super();
  }

  override async health() {
    return {
      ok: true,
      detail: "echo workerd fetch plugin ready",
    };
  }

  override async diagnose() {
    const probe = await probeExampleFetch();
    return {
      lines: [
        "echo_workerd_fetch: ok",
        probe.detail,
        probe.allowed
          ? "allowlist probe: passed (response received)"
          : "allowlist probe: failed (no response — treat as deny/block)",
      ],
    };
  }

  override async onEvent(event: DomainEvent): Promise<EventResult> {
    const host = (this.pluginEnv as { HOST?: { notify?: (e: unknown) => Promise<void> } })
      ?.HOST;
    if (host?.notify) {
      await host.notify({
        type: "plugin_log",
        payload: {
          level: "info",
          message: `echo_workerd_fetch saw event ${event.eventType}`,
        },
      });
    }
    return { kind: "ack" };
  }
}

/**
 * Echo Fetch — workerd guest that requests outbound `*.example.com` and probes
 * `https://www.example.com/` from diagnose / CLI.
 */
export default class EchoFetchPlugin extends BookclerkPlugin {
  async describe(): Promise<PluginDescribe> {
    return {
      apiVersion: PRODUCT_API_VERSION,
      id: PLUGIN_ID,
      kind: "integration",
      displayName: "Echo Fetch (workerd)",
      rpcFeatures: [FEATURE_SCALAR_LIMITS],
      scalarLimits: {
        maxScalarBytes: MAX_SCALAR_BYTES,
        maxStreamWindowBytes: MAX_STREAM_WINDOW_BYTES,
        maxListPage: MAX_LIST_PAGE,
      },
      supportedRoles: ["integration"],
    };
  }

  integration() {
    return new EchoFetchIntegration(this.env);
  }

  override async cliDescribe(): Promise<string> {
    return JSON.stringify(CLI);
  }

  override async cliInvoke(paramsJson: string): Promise<string> {
    const params = JSON.parse(paramsJson || "{}") as {
      command?: string;
      args?: { message?: string };
    };
    if (params.command === "ping") {
      const message = params.args?.message ?? "hi";
      return JSON.stringify({
        exitCode: 0,
        stdout: `pong: ${message}\n`,
        json: { pong: message },
      });
    }
    if (params.command === "fetch-example") {
      const probe = await probeExampleFetch();
      return JSON.stringify({
        exitCode: probe.allowed ? 0 : 1,
        stdout: `${probe.detail}\n`,
        json: { allowed: probe.allowed, detail: probe.detail, url: EXAMPLE_URL },
      });
    }
    return JSON.stringify({
      exitCode: 2,
      stderr: `unknown command ${params.command ?? ""}`,
    });
  }
}
