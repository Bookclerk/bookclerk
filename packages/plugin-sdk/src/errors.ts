/**
 * Typed plugin error shared by the author runtime (`plugin.ts`) and the
 * adapter dispatcher (`invoke.ts`). Kept in its own module so both can extend
 * it without an import cycle.
 *
 * @module
 */

import { MAX_SCALAR_BYTES, PLUGIN_ERROR_CODES } from "./abi.js";

const KNOWN_ERROR_CODES: ReadonlySet<string> = new Set<string>(PLUGIN_ERROR_CODES);

/** Longest `details` text the SDK parses (the ABI scalar ceiling). */
const MAX_DETAILS_BYTES: number = MAX_SCALAR_BYTES;

/**
 * A negotiated SQL-v1 cap that a request or its result exceeded
 * (`limit_exceeded` errors).
 */
export interface LimitExceeded {
  /** Capability field name as advertised (`maxResultBytes`, `maxRequestBytes`, ...). */
  limit: string;
  /** The negotiated cap. */
  cap: number;
  /** The observed value that exceeded `cap`. */
  used: number;
}

/** Message prefix every limit error starts with (shared with the host). */
const LIMIT_MESSAGE_PREFIX = "limit exceeded: ";

/**
 * Canonical message for a limit error: `limit exceeded: maxResultBytes is 262144; used 300000`.
 *
 * @param limit - Exceeded cap.
 * @returns Message text the host and every SDK parse back with {@link parseLimitExceeded}.
 */
export function limitExceededMessage(limit: LimitExceeded): string {
  return `${LIMIT_MESSAGE_PREFIX}${limit.limit} is ${limit.cap}; used ${limit.used}`;
}

/**
 * Parses a message produced by {@link limitExceededMessage}, possibly wrapped
 * in further text.
 *
 * @param message - Error text.
 * @returns The exceeded cap, or `undefined` when the text is not a limit error.
 */
export function parseLimitExceeded(message: string): LimitExceeded | undefined {
  const start = message.indexOf(LIMIT_MESSAGE_PREFIX);
  if (start < 0) return undefined;
  const match = /^([A-Za-z0-9_]+) is (\d+); used (\d+)/.exec(
    message.slice(start + LIMIT_MESSAGE_PREFIX.length),
  );
  if (!match) return undefined;
  const cap = Number(match[2]);
  const used = Number(match[3]);
  if (!Number.isSafeInteger(cap) || !Number.isSafeInteger(used)) return undefined;
  return { limit: match[1] ?? "", cap, used };
}

function limitFromDetails(details: Readonly<Record<string, unknown>>): LimitExceeded | undefined {
  const { limit, cap, used } = details;
  if (typeof limit !== "string" || typeof cap !== "number" || typeof used !== "number") {
    return undefined;
  }
  return { limit, cap, used };
}

function parseDetails(text: string): Readonly<Record<string, unknown>> | undefined {
  if (text === "" || text.length > MAX_DETAILS_BYTES) return undefined;
  try {
    const parsed: unknown = JSON.parse(text);
    if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)) {
      return Object.freeze({ ...(parsed as Record<string, unknown>) });
    }
  } catch {
    // Malformed details are dropped; the code and message still stand.
  }
  return undefined;
}

/** Thrown by the SDK when a wire union carries `err`. Unknown codes are kept. */
export class PluginError extends Error {
  /** Known `PluginErrorCode` wire string, or `unknown`. */
  readonly code: string;
  /** Raw wire code, including codes this SDK does not know. */
  readonly wireCode: string;
  /** Structured detail object from the wire (`details` JSON), when the error carries one. */
  readonly details: Readonly<Record<string, unknown>> | undefined;

  /**
   * @param code - Wire error code.
   * @param message - Operator-facing error text.
   * @param details - Detail object, or its JSON text as carried on the wire.
   */
  constructor(code: string, message: string, details?: Readonly<Record<string, unknown>> | string) {
    super(message);
    this.name = "PluginError";
    this.wireCode = code;
    this.code = KNOWN_ERROR_CODES.has(code) ? code : "unknown";
    this.details =
      typeof details === "string"
        ? parseDetails(details)
        : details === undefined
          ? undefined
          : Object.freeze({ ...details });
  }

  /**
   * Construct a {@link PluginError} from a Cap'n Proto / JSON wire code.
   *
   * Unknown codes become `unknown` on {@link PluginError.code} while
   * {@link PluginError.wireCode} keeps the raw value.
   *
   * @param code - Wire error code (known or unknown).
   * @param message - Operator-facing error text.
   * @param details - `details` JSON text from the wire (empty when none).
   * @returns Typed plugin error.
   */
  static fromWire(code: string, message: string, details = ""): PluginError {
    return new PluginError(code, message, details);
  }

  /**
   * Builds a `limit_exceeded` error whose message and `details` both carry
   * the exceeded cap.
   *
   * @param limit - Exceeded cap.
   * @returns Typed plugin error.
   */
  static limitExceeded(limit: LimitExceeded): PluginError {
    return new PluginError("limit_exceeded", limitExceededMessage(limit), { ...limit });
  }

  /**
   * The exceeded cap when this is a `limit_exceeded` error.
   *
   * @returns The cap from `details` first, then from the canonical message;
   * `undefined` for every other code.
   */
  get limit(): LimitExceeded | undefined {
    if (this.wireCode !== "limit_exceeded") return undefined;
    return (this.details && limitFromDetails(this.details)) ?? parseLimitExceeded(this.message);
  }

  /**
   * `details` as the JSON text the wire carries.
   *
   * @returns JSON object text, or an empty string when the error has none.
   */
  get detailsText(): string {
    return this.details === undefined ? "" : JSON.stringify(this.details);
  }
}
