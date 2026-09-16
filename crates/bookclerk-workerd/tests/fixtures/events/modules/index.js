/**
 * Event contract fixture: Integration.onEvent result variants.
 */

import {
  BookclerkPlugin,
  Integration,
  PRODUCT_API_VERSION,
  FEATURE_SCALAR_LIMITS,
} from "@bookclerk/plugin-sdk/workerd";

class EventIntegration extends Integration {
  async health() {
    return { ok: true, detail: "events_fixture ready" };
  }

  async onEvent(event) {
    const type = event?.eventType || event?.event_type || event?.type || "";
    switch (type) {
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

export default class EventPlugin extends BookclerkPlugin {
  async describe() {
    return {
      apiVersion: PRODUCT_API_VERSION,
      id: "events_fixture",
      kind: "integration",
      displayName: "Event contract fixture",
      rpcFeatures: [FEATURE_SCALAR_LIMITS],
      scalarLimits: {
        maxScalarBytes: 262144,
        maxStreamWindowBytes: 1048576,
        maxListPage: 256,
      },
      supportedRoles: ["integration"],
    };
  }

  integration() {
    return new EventIntegration();
  }
}
