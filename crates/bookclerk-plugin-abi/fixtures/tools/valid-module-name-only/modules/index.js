import { BookclerkEntrypoint, CliEntrypoint } from "@bookclerk/plugin-sdk/workerd";

export class Cli extends CliEntrypoint {
  async describe() {
    return { commands: [] };
  }
}

export default class ToolsFixture extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Echo workerd tools fixture" };
  }
}
