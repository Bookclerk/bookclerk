import {
  BookclerkPlugin,
  type CliInvokeParams,
  type CliInvokeResult,
  type DiagnoseResult,
  type HandshakeParams,
  type HandshakeResult,
  type HostToPluginEvent,
} from "@bookclerk/plugin-sdk/workerd";

/**
 * Echo — branded BookclerkPlugin, not raw WorkerEntrypoint.
 *
 * Authoring source for the workerd guest. Ship `modules/index.js` (built or
 * hand-maintained MVP sibling) beside `plugin.toml`.
 */
export default class EchoPlugin extends BookclerkPlugin {
  async handshake(_params: HandshakeParams): Promise<HandshakeResult> {
    return {
      apiVersion: 1,
      id: "echo_workerd_ts",
      kind: "integration",
      displayName: "Echo Integration (workerd TypeScript)",
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
        ],
      },
    };
  }

  async diagnose(): Promise<DiagnoseResult> {
    return { lines: ["echo: ok"] };
  }

  async onEvent(event: HostToPluginEvent): Promise<void> {
    if (event.type === "book_acquired") {
      await this.env.HOST.notify({
        type: "plugin_log",
        payload: {
          level: "info",
          message: `echo saw book_acquired titleId=${event.payload.titleId}`,
        },
      });
    }
  }

  async cliInvoke(params: CliInvokeParams): Promise<CliInvokeResult> {
    if (params.command !== "ping") {
      return { exitCode: 2, stderr: `unknown command ${params.command}` };
    }
    return {
      exitCode: 0,
      stdout: `pong: ${String(params.args?.message ?? "hi")}\n`,
    };
  }
}
