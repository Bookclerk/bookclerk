import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";

export default class ToolsFixture extends BookclerkPlugin {
  async describe() {
    return {
      apiVersion: 2,
      id: "echo-workerd-tools",
      kind: "integration",
      rpcFeatures: [],
      scalarLimits: {
        maxScalarBytes: 262144,
        maxStreamWindowBytes: 1048576,
        maxListPage: 256,
      },
      supportedRoles: ["integration"],
      metadataJson: '{"capabilities": ["health"]}',
    };
  }
}
