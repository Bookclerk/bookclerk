import {
  BookclerkPlugin,
  type CliInvokeParams,
  type CliInvokeResult,
  type DiagnoseResult,
  type HandshakeParams,
  type HandshakeResult,
  type HostToPluginEvent,
} from "@bookclerk/plugin-sdk/workerd";

const EXAMPLE_URL = "https://www.example.com/";

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

/**
 * Echo Fetch — workerd guest that requests outbound `*.example.com` and probes
 * `https://www.example.com/` from diagnose / CLI.
 */
export default class EchoFetchPlugin extends BookclerkPlugin {
  async handshake(_params: HandshakeParams): Promise<HandshakeResult> {
    return {
      apiVersion: 1,
      id: "echo_workerd_fetch",
      kind: "integration",
      displayName: "Echo Fetch (workerd)",
      capabilities: ["health", "diagnose", "onEvent", "cli"],
      cli: {
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
      },
    };
  }

  async diagnose(): Promise<DiagnoseResult> {
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

  async onEvent(event: HostToPluginEvent): Promise<void> {
    if (event.type === "book_acquired") {
      await this.env.HOST.notify({
        type: "plugin_log",
        payload: {
          level: "info",
          message: `echo_workerd_fetch saw book_acquired titleId=${event.payload.titleId}`,
        },
      });
    }
  }

  async cliInvoke(params: CliInvokeParams): Promise<CliInvokeResult> {
    if (params.command === "ping") {
      return {
        exitCode: 0,
        stdout: `pong: ${String(params.args?.message ?? "hi")}\n`,
      };
    }
    if (params.command === "fetch-example") {
      const probe = await probeExampleFetch();
      // Exit 0 when egress allowed a Response; HTTP status is informational only.
      return {
        exitCode: probe.allowed ? 0 : 1,
        stdout: `${probe.detail}\n`,
        json: { allowed: probe.allowed, detail: probe.detail, url: EXAMPLE_URL },
      };
    }
    return { exitCode: 2, stderr: `unknown command ${params.command}` };
  }
}
