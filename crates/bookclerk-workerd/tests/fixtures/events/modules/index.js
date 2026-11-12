/**
 * Event contract fixture: `event(batch)` outcome variants recorded on the
 * delivered `EventMessage` (ack / retry / reject / deadLetter / suspend).
 */

import { BookclerkEntrypoint } from "@bookclerk/plugin-sdk/workerd";

export default class EventPlugin extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Event contract fixture" };
  }

  async event(batch) {
    for (const msg of batch.messages) {
      switch (msg.type) {
        case "test_retry":
          msg.retry({ retryAt: 1, reason: "echo retry" });
          break;
        case "test_reject":
          msg.reject("echo reject");
          break;
        case "test_dead_letter":
          msg.deadLetter("echo dead letter");
          break;
        case "test_suspend":
          msg.suspend({ checkpoint: { n: 1 }, checkpointSchemaVersion: 1, wakeAt: 1 });
          break;
        case "test_publish": {
          // Publish through the granted `EVENTS` binding and report the
          // host's `PublishOk` (or the failure) as the reject reason so the
          // contract test can assert it end to end.
          const events = this.env.EVENTS;
          if (!events) {
            msg.reject("no EVENTS binding");
            break;
          }
          try {
            const ok = await events.publish({
              eventType: "fixture_pinged",
              deduplicationKey: `pinged:${msg.id}`,
              correlationId: msg.correlationId,
              payload: { from: msg.id, n: msg.json().n ?? 0 },
            });
            msg.reject(JSON.stringify(ok));
          } catch (err) {
            msg.reject(`publish failed: ${err.code ?? "unknown"}: ${err.message}`);
          }
          break;
        }
        case "test_database": {
          // Named `[[databases]]` binding: the adapter installs `env.DB`
          // over the granted `/db/execute` channel; report the first row
          // (or the failure) as the reject reason for the contract test.
          const db = this.env.DB;
          if (!db) {
            msg.reject("no DB binding");
            break;
          }
          try {
            const row = await db
              .prepare("SELECT ? AS n")
              .bind({ kind: "int64", value: 41n })
              .first();
            msg.reject(JSON.stringify(row, (_k, v) => (typeof v === "bigint" ? Number(v) : v)));
          } catch (err) {
            msg.reject(`database failed: ${err.code ?? "unknown"}: ${err.message}`);
          }
          break;
        }
        case "test_read": {
          // Read past events through the granted `EVENTS` binding and report
          // the page (payloads decoded) as the reject reason.
          try {
            const page = await this.env.EVENTS.read({
              after: "v1.start",
              types: ["fixture_pinged"],
              limit: 2,
            });
            msg.reject(
              JSON.stringify({
                ...page,
                events: page.events.map((e) => ({
                  id: e.eventId,
                  attempt: e.deliveryAttempt,
                  payload: new TextDecoder().decode(e.payload),
                })),
              }),
            );
          } catch (err) {
            msg.reject(`read failed: ${err.code ?? "unknown"}: ${err.message}`);
          }
          break;
        }
        case "test_read_since": {
          try {
            const page = await this.env.EVENTS.read({ since: new Date(5000) });
            msg.reject(`read ok: ${page.events.length}`);
          } catch (err) {
            msg.reject(`read failed: ${err.code ?? "unknown"}`);
          }
          break;
        }
        case "test_read_forbidden": {
          try {
            await this.env.EVENTS.read({ types: ["not_granted"] });
            msg.reject("unexpected success");
          } catch (err) {
            msg.reject(`read failed: ${err.code ?? "unknown"}`);
          }
          break;
        }
        case "test_publish_forbidden": {
          try {
            await this.env.EVENTS.publish({ eventType: "not_granted", payload: {} });
            msg.reject("unexpected success");
          } catch (err) {
            msg.reject(`publish failed: ${err.code ?? "unknown"}`);
          }
          break;
        }
        default:
          msg.ack();
      }
    }
  }
}
