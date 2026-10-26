/**
 * Narrowing helpers for parsed JSON and other untrusted values.
 */

/** A JSON object: string keys with values of unknown type. */
export type JsonObject = Record<string, unknown>;

/**
 * Whether a value is a plain object (not null, not an array).
 *
 * @param value - Any value, typically from `JSON.parse`.
 * @returns True when its fields can be read by name.
 */
export function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * A value as an object, or an empty object when it is not one.
 *
 * @param value - Any value, typically from `JSON.parse`.
 * @returns The value, or `{}`.
 */
export function asObject(value: unknown): JsonObject {
  return isObject(value) ? value : {};
}

/**
 * Whether a value is a safe integer (`Number.isSafeInteger` as a type guard).
 *
 * @param value - Any value.
 * @returns True for a number that is an integer within ±(2^53 − 1).
 */
export function isSafeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value);
}

/**
 * Whether a value is any non-null object, arrays included (whose named
 * fields read as undefined): the `typeof v === "object" && v !== null`
 * test, as a type guard.
 *
 * @param value - Any value.
 * @returns True when its fields can be read.
 */
export function isRecordLike(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null;
}

/**
 * A field of a value, like `value?.[key]` for parsed JSON: undefined when
 * the value is not an object.
 *
 * @param value - Any value, typically from `JSON.parse`.
 * @param key - Field name.
 * @returns The field, or undefined.
 */
export function prop(value: unknown, key: string): unknown {
  return isRecordLike(value) ? value[key] : undefined;
}
