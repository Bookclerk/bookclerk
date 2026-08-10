import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";

export default class ToolsFixture extends BookclerkPlugin {
  async handshake() {
    return {
      apiVersion: 1,
      id: "echo-workerd-tools",
      kind: "integration",
      capabilities: ["health"],
    };
  }
}
