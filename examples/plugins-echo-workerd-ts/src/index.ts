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

const PLUGIN_ID = "echo_workerd_ts";

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
  ],
};

class EchoIntegration extends Integration {
  constructor(private readonly pluginEnv: BookclerkPlugin["env"]) {
    super();
  }

  override async health() {
    return {
      ok: true,
      detail: "echo workerd plugin ready",
    };
  }

  override async diagnose() {
    return { lines: ["echo: ok"] };
  }

  override async onEvent(event: DomainEvent): Promise<EventResult> {
    const payload = event.payload;
    let titleId = "";
    if (payload && payload.byteLength > 0) {
      try {
        const parsed = JSON.parse(new TextDecoder().decode(payload)) as {
          titleId?: string;
          payload?: { titleId?: string };
        };
        titleId = parsed.titleId ?? parsed.payload?.titleId ?? "";
      } catch {
        titleId = "";
      }
    }
    const host = (this.pluginEnv as { HOST?: { notify?: (event: unknown) => Promise<void> } })
      ?.HOST;
    if (host?.notify) {
      await host.notify({
        type: "plugin_log",
        payload: {
          level: "info",
          message: `echo saw ${event.eventType} titleId=${titleId}`,
        },
      });
    }
    switch (event.eventType) {
      case "test_retry":
        return { kind: "retry", retryAtUnixMs: 1, reason: "echo retry" };
      case "test_reject":
        return { kind: "reject", reason: "echo reject" };
      case "test_dead_letter":
        return { kind: "deadLetter", reason: "echo dead letter" };
      case "test_suspend":
        return {
          kind: "suspended",
          checkpointJson: "{\"n\":1}",
          checkpointSchemaVersion: 1,
          wakeAtUnixMs: 1,
        };
      default:
        return { kind: "ack" };
    }
  }
}

/**
 * Echo — branded BookclerkPlugin v2 (`describe` + `integration` RpcTarget).
 *
 * Authoring source for the workerd guest. Ship `modules/index.js` (built or
 * hand-maintained MVP sibling) beside `plugin.toml`.
 */
export default class EchoPlugin extends BookclerkPlugin {
  async describe(): Promise<PluginDescribe> {
    return {
      apiVersion: PRODUCT_API_VERSION,
      id: PLUGIN_ID,
      kind: "integration",
      displayName: "Echo Integration (workerd TypeScript)",
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
    return new EchoIntegration(this.env);
  }

  override async cliDescribe(): Promise<string> {
    return JSON.stringify(CLI);
  }

  override async cliInvoke(paramsJson: string): Promise<string> {
    const params = JSON.parse(paramsJson || "{}") as {
      command?: string;
      args?: { message?: string };
    };
    if (params.command !== "ping") {
      return JSON.stringify({
        exitCode: 2,
        stderr: `unknown command ${params.command ?? ""}`,
      });
    }
    const message = params.args?.message ?? "hi";
    return JSON.stringify({
      exitCode: 0,
      stdout: `pong: ${message}\n`,
      json: { pong: message },
    });
  }
}
