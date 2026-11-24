/**
 * Backblaze B2 event notifications and notification-rule replies, read into
 * checked values, sans-IO.
 *
 * A B2 bucket announces object writes, deletes and hide markers by posting
 * signed batches of events to a webhook (an event notification rule). The
 * host checks the signature and stores each request, and the destination
 * reads the stored bodies to serve `changes`. The destination also reads the
 * bucket's rules back from `b2_get_bucket_notification_rules` and
 * `b2_set_bucket_notification_rules` to keep its own rules in place. Both
 * come from outside Bookclerk, so this module is the boundary that reads
 * them, fuzzed by `test/fuzz.test.ts`.
 *
 * Every event the feed reports becomes a `changed` entry with no object
 * details: B2 may deliver events late, twice or out of order, and an event
 * for one version says little about the name's current version (deleting a
 * version can uncover an older one), so the host probes each key. Events of
 * other types, for another bucket, or for names outside the destination
 * prefix are skipped. Anything else unreadable is an error: the caller
 * counts the delivery as a change it could not read.
 */

import type { ObjectChange } from "@bookclerk/plugin-sdk/workerd";
import { B2ReplyError, MAX_B2_REPLY_BYTES, fileNameProblem, isCount, isObject } from "./b2-api.ts";
import { sha256Hex } from "./hash.ts";

/** Largest webhook body read, in bytes (the host's own webhook cap). */
export const MAX_EVENT_MESSAGE_BYTES = 1024 * 1024;
/**
 * Most events read from one webhook body. B2 batches at most 50; the cap
 * bounds what a crafted body can make the feed hold.
 */
export const MAX_B2_EVENTS = 256;
/** Most rules one rules reply may list. B2 allows 25 per bucket. */
export const MAX_NOTIFICATION_RULES = 100;
/** Length of a rule's signing secret, in characters. */
export const SIGNING_SECRET_LEN = 32;
/** Events per webhook request the destination's rules ask for (B2's maximum). */
export const EVENTS_PER_BATCH = 50;

/**
 * The event-type categories the feed reports, each covered by one of the
 * destination's rules (B2 refuses a rule that mixes categories), with the
 * suffix of that rule's name.
 */
export const EVENT_CATEGORIES: readonly (readonly [string, string])[] = [
  ["b2:ObjectCreated", "created"],
  ["b2:ObjectDeleted", "deleted"],
  ["b2:HideMarkerCreated", "hidden"],
];

/** Which objects a destination owns. */
export interface B2EventScope {
  /** Bucket name; events for other buckets are skipped. */
  bucket: string;
  /**
   * Normalized key prefix (empty, or ending in `/`); it is stripped from
   * every name, and names outside it are skipped.
   */
  prefix: string;
}

/** One change read from a webhook body. */
export interface ParsedEvent {
  /** The change, keyed below the destination prefix. */
  change: ObjectChange;
  /**
   * The event time as twenty zero-padded digits of Unix milliseconds (so
   * times compare as text), or `null` when the event states none.
   */
  time: string | null;
}

/** Why a webhook body could not be read. */
export type EventParseErrorKind = "too_large" | "too_many_records" | "malformed" | "bad_key";

/** A webhook body the feed cannot read. */
export class EventParseError extends Error {
  /** What was wrong. */
  readonly kind: EventParseErrorKind;

  /**
   * Creates the error.
   *
   * @param kind - What was wrong.
   * @param message - Details.
   */
  constructor(kind: EventParseErrorKind, message: string) {
    super(message);
    this.name = "EventParseError";
    this.kind = kind;
  }
}

/** Strict UTF-8 decoder: a body that is not UTF-8 is malformed. */
const STRICT_UTF8 = new TextDecoder("utf-8", { fatal: true });

/**
 * Whether the feed reports events of `eventType`: one of
 * {@link EVENT_CATEGORIES} with a non-empty third part. Test events,
 * live-read uploads and types B2 adds in new categories are not reported.
 *
 * @param eventType - The event's type, such as `b2:ObjectCreated:Upload`.
 * @returns Whether it is reported.
 */
function reported(eventType: string): boolean {
  const first = eventType.indexOf(":");
  if (first < 0) return false;
  const second = eventType.indexOf(":", first + 1);
  if (second < 0) return false;
  const vendor = eventType.slice(0, first);
  const category = eventType.slice(first + 1, second);
  const detail = eventType.slice(second + 1);
  return (
    vendor === "b2" &&
    detail.length > 0 &&
    EVENT_CATEGORIES.some(([known]) => known === `b2:${category}`)
  );
}

/**
 * Parses `bytes` as one JSON value.
 *
 * @param bytes - The body.
 * @returns The value.
 * @throws {Error} With the reason when it is not UTF-8 or not JSON.
 */
function parseJson(bytes: Uint8Array): unknown {
  return JSON.parse(STRICT_UTF8.decode(bytes)) as unknown;
}

/**
 * Reads one webhook body into changes for `scope`, in the order it lists
 * them.
 *
 * A repeated JSON member counts once, as its last value, and the body and
 * each event must be JSON objects.
 *
 * @param body - The request body as the host stored it.
 * @param scope - The destination's bucket and key prefix.
 * @returns A `changed` entry per reported event under `scope`.
 * @throws {EventParseError} When the body is too large, is not a list of
 *   events, lists too many, or names an unreadable object.
 */
export function parseB2Events(body: Uint8Array, scope: B2EventScope): ParsedEvent[] {
  if (body.length > MAX_EVENT_MESSAGE_BYTES) {
    throw new EventParseError(
      "too_large",
      `B2 events: the body is over ${MAX_EVENT_MESSAGE_BYTES} bytes`,
    );
  }
  const malformed = (what: string): EventParseError =>
    new EventParseError("malformed", `B2 events: ${what}`);
  let value: unknown;
  try {
    value = parseJson(body);
  } catch (err) {
    throw malformed((err as Error).message);
  }
  const events = isObject(value) ? value["events"] : undefined;
  if (!Array.isArray(events)) throw malformed("the body is an object with an events list");
  if (events.length > MAX_B2_EVENTS) {
    throw new EventParseError("too_many_records", `B2 events: more than ${MAX_B2_EVENTS} events`);
  }
  const out: ParsedEvent[] = [];
  for (const event of events as unknown[]) {
    if (!isObject(event)) throw malformed("each event is an object");
    const text = (field: string): string => {
      const found = event[field];
      if (typeof found !== "string") throw malformed(`an event needs its ${field}`);
      return found;
    };
    if (!reported(text("eventType")) || text("bucketName") !== scope.bucket) continue;
    const name = text("objectName");
    // Scope comes before validation: a broken name outside this destination
    // must not fail the delivery.
    if (!name.startsWith(scope.prefix)) continue;
    const key = name.slice(scope.prefix.length);
    if (key.length === 0) continue;
    if (fileNameProblem(name) !== null) {
      throw new EventParseError("bad_key", "B2 events: an event names an unreadable object");
    }
    const stamp = event["eventTimestamp"];
    let time: string | null = null;
    if (stamp !== undefined && stamp !== null) {
      if (!isCount(stamp)) throw malformed("an event time is a whole number");
      time = String(stamp).padStart(20, "0");
    }
    out.push({ change: { key, kind: "changed" }, time });
  }
  return out;
}

/**
 * The shared secret B2 signs a rule's webhook requests with: 32 ASCII
 * letters or digits, as B2 requires. It never prints itself.
 */
export class B2SigningSecret {
  /** The secret itself. */
  readonly #secret: string;

  /**
   * Wraps a checked secret.
   *
   * @param secret - The secret.
   */
  private constructor(secret: string) {
    this.#secret = secret;
  }

  /**
   * Checks a secret.
   *
   * @param secret - The candidate.
   * @returns The secret, or why it is refused.
   */
  static parse(secret: string): B2SigningSecret | string {
    if (secret.length === SIGNING_SECRET_LEN && /^[A-Za-z0-9]+$/.test(secret)) {
      return new B2SigningSecret(secret);
    }
    return `the webhook signing secret must be ${SIGNING_SECRET_LEN} letters or digits`;
  }

  /**
   * The secret itself, for the rule B2 stores.
   *
   * @returns The secret.
   */
  expose(): string {
    return this.#secret;
  }

  /**
   * A redacted rendering, so a log line never carries the secret.
   *
   * @returns `B2SigningSecret(<redacted>)`.
   */
  toString(): string {
    return "B2SigningSecret(<redacted>)";
  }

  /**
   * A redacted JSON rendering.
   *
   * @returns `"<redacted>"`.
   */
  toJSON(): string {
    return "<redacted>";
  }
}

/** One event notification rule as B2 lists it. */
export interface NotificationRule {
  /** The rule's name, unique in the bucket. */
  name: string;
  /** The event types it matches. */
  eventTypes: string[];
  /** The name prefix it matches; empty for every name. */
  objectNamePrefix: string;
  /** Whether it is turned on. */
  isEnabled: boolean;
  /**
   * Whether B2 suspended it after failed deliveries; a suspended rule sends
   * nothing and can only be deleted.
   */
  isSuspended: boolean;
  /** The webhook URL, for a webhook target. */
  url: string | null;
  /** The signing secret, when the rule has one and it is well formed. */
  signingSecret: B2SigningSecret | null;
  /** Events per request, when the listing gives it. */
  maxEventsPerBatch: number | null;
  /**
   * The rule as listed, without the fields B2 computes, so a rule that is
   * not the destination's own can be sent back unchanged (setting rules
   * replaces them all).
   */
  raw: Record<string, unknown>;
}

/**
 * Reads a `b2_get_bucket_notification_rules` or
 * `b2_set_bucket_notification_rules` reply.
 *
 * @param bytes - The reply body.
 * @returns The bucket's rules, in the order the reply lists them.
 * @throws {B2ReplyError} For an oversized reply, one that is not a JSON
 *   object with a list of rule objects, more than
 *   {@link MAX_NOTIFICATION_RULES} rules, a rule without a name of 1 to 63
 *   bytes, or two rules with one name.
 */
export function parseNotificationRules(bytes: Uint8Array): NotificationRule[] {
  if (bytes.length > MAX_B2_REPLY_BYTES) {
    throw new B2ReplyError("too_large", `B2 reply is over ${MAX_B2_REPLY_BYTES} bytes`);
  }
  const malformed = (what: string): B2ReplyError =>
    new B2ReplyError("malformed", `notification rules: ${what}`);
  let value: unknown;
  try {
    value = parseJson(bytes);
  } catch (err) {
    throw malformed((err as Error).message);
  }
  const rules = isObject(value) ? value["eventNotificationRules"] : undefined;
  if (!Array.isArray(rules)) throw malformed("the reply is an object with a list of rules");
  if (rules.length > MAX_NOTIFICATION_RULES) throw malformed("too many rules");
  const out: NotificationRule[] = [];
  for (const rule of rules as unknown[]) {
    if (!isObject(rule)) throw malformed("each rule is an object");
    const name = rule["name"];
    if (typeof name !== "string" || name.length === 0 || ENCODER.encode(name).length > 63) {
      throw malformed("each rule has a name of 1 to 63 bytes");
    }
    if (out.some((seen) => seen.name === name)) throw malformed("two rules share a name");
    const target = isObject(rule["targetConfiguration"]) ? rule["targetConfiguration"] : null;
    const targetText = (field: string): string | null => {
      const found = target?.[field];
      return typeof found === "string" ? found : null;
    };
    const raw: Record<string, unknown> = { ...rule };
    delete raw["isSuspended"];
    delete raw["suspensionReason"];
    const types = rule["eventTypes"];
    const secretText = targetText("hmacSha256SigningSecret");
    const secret = secretText === null ? null : B2SigningSecret.parse(secretText);
    const batch = target?.["maxEventsPerBatch"];
    out.push({
      name,
      eventTypes: Array.isArray(types)
        ? (types as unknown[]).filter((type): type is string => typeof type === "string")
        : [],
      objectNamePrefix:
        typeof rule["objectNamePrefix"] === "string" ? rule["objectNamePrefix"] : "",
      isEnabled: rule["isEnabled"] === true,
      isSuspended: rule["isSuspended"] === true,
      url: targetText("url"),
      signingSecret: secret instanceof B2SigningSecret ? secret : null,
      maxEventsPerBatch: isCount(batch) ? batch : null,
      raw,
    });
  }
  return out;
}

/** Shared encoder for byte lengths. */
const ENCODER = new TextEncoder();

/**
 * The names of the destination's rules for `prefix`: `bookclerk-`, 16 hex
 * digits of the prefix's SHA-256, and the category, one per entry of
 * {@link EVENT_CATEGORIES}. B2 refuses two rules for one prefix and event
 * type, so the names follow the prefix, not the instance.
 *
 * @param prefix - The destination's normalized prefix.
 * @returns The three rule names.
 */
export async function ruleNames(prefix: string): Promise<string[]> {
  const tag = (await sha256Hex(prefix)).slice(0, 16);
  return EVENT_CATEGORIES.map(([, suffix]) => `bookclerk-${tag}-${suffix}`);
}

/**
 * The destination's rules for `prefix`, posting to `url` signed with
 * `secret`.
 *
 * @param prefix - The destination's normalized prefix.
 * @param url - The webhook URL.
 * @param secret - The signing secret.
 * @returns The rules, as B2 takes them.
 */
export async function ownRules(
  prefix: string,
  url: string,
  secret: B2SigningSecret,
): Promise<Record<string, unknown>[]> {
  const names = await ruleNames(prefix);
  return EVENT_CATEGORIES.map(([category], at) => ({
    name: names[at],
    eventTypes: [`${category}:*`],
    isEnabled: true,
    objectNamePrefix: prefix,
    targetConfiguration: {
      targetType: "webhook",
      url,
      hmacSha256SigningSecret: secret.expose(),
      customHeaders: [],
      maxEventsPerBatch: EVENTS_PER_BATCH,
    },
  }));
}

/**
 * True when `existing` holds the destination's rules for `prefix` as
 * {@link ownRules} would write them, turned on and not suspended.
 *
 * B2 does not document whether a listed rule repeats its signing secret and
 * batch size, so a field the listing leaves out is not held against the
 * rule; one it gives must match.
 *
 * @param existing - The bucket's rules.
 * @param prefix - The destination's normalized prefix.
 * @param url - The webhook URL.
 * @param secret - The signing secret.
 * @returns Whether the rules are in place.
 */
export async function ownRulesInPlace(
  existing: readonly NotificationRule[],
  prefix: string,
  url: string,
  secret: B2SigningSecret,
): Promise<boolean> {
  const names = await ruleNames(prefix);
  return EVENT_CATEGORIES.every(([category], at) =>
    existing.some((rule) => {
      const target = rule.raw["targetConfiguration"];
      const listedSecret = isObject(target) ? target["hmacSha256SigningSecret"] : undefined;
      return (
        rule.name === names[at] &&
        rule.isEnabled &&
        !rule.isSuspended &&
        rule.eventTypes.length === 1 &&
        rule.eventTypes[0] === `${category}:*` &&
        rule.objectNamePrefix === prefix &&
        rule.url === url &&
        (listedSecret === undefined || listedSecret === secret.expose()) &&
        (rule.maxEventsPerBatch === null || rule.maxEventsPerBatch === EVENTS_PER_BATCH)
      );
    }),
  );
}

/**
 * The rules to send back to keep every rule in `existing` except the
 * destination's own for `prefix`, then add `own` (empty to remove them). A
 * suspended rule of the destination's is dropped too: B2 lets it only be
 * deleted.
 *
 * @param existing - The bucket's rules.
 * @param prefix - The destination's normalized prefix.
 * @param own - The destination's rules to add.
 * @returns The full rule list to set.
 */
export async function rulesWith(
  existing: readonly NotificationRule[],
  prefix: string,
  own: Record<string, unknown>[],
): Promise<Record<string, unknown>[]> {
  const names = await ruleNames(prefix);
  return [
    ...existing.filter((rule) => !names.includes(rule.name)).map((rule) => ({ ...rule.raw })),
    ...own,
  ];
}
