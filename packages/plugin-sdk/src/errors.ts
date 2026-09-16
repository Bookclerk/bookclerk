/**
 * Typed plugin error shared by the author runtime (`plugin.ts`) and the
 * adapter dispatcher (`invoke.ts`). Kept in its own module so both can extend
 * it without an import cycle.
 *
 * @module
 */

import { PLUGIN_ERROR_CODES } from "./abi.js";

const KNOWN_ERROR_CODES: ReadonlySet<string> = new Set<string>(PLUGIN_ERROR_CODES);

/** Thrown by the SDK when a wire union carries `err`. Unknown codes are kept. */
export class PluginError extends Error {
  /** Known `PluginErrorCode` wire string, or `unknown`. */
  readonly code: string;
  /** Raw wire code, including codes this SDK does not know. */
  readonly wireCode: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "PluginError";
    this.wireCode = code;
    this.code = KNOWN_ERROR_CODES.has(code) ? code : "unknown";
  }

  /**
   * Construct a {@link PluginError} from a Cap'n Proto / JSON wire code.
   *
   * Unknown codes become `unknown` on {@link PluginError.code} while
   * {@link PluginError.wireCode} keeps the raw value.
   *
   * @param code - Wire error code (known or unknown).
   * @param message - Operator-facing error text.
   * @returns Typed plugin error.
   */
  static fromWire(code: string, message: string): PluginError {
    return new PluginError(code, message);
  }
}
