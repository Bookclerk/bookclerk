/**
 * Checks the visual suite's sample API answers against the daemon's
 * committed OpenAPI document (crates/bookclerkd/openapi.json), so a sample
 * that no longer matches what the server sends fails the check instead of
 * screenshotting a page the real app would never show.
 *
 * Only the parts of JSON Schema the document uses are understood: `$ref`,
 * `type` (one or several), `enum`, `properties` with `required`,
 * `additionalProperties`, `items`, `oneOf` / `anyOf` / `allOf`, `minimum`
 * and `format` (ignored).
 */
import { readFileSync } from "node:fs";
import path from "node:path";

/** A schema object from the document. */
type Schema = {
  /** Reference to `#/components/schemas/<name>`. */
  $ref?: string;
  /** One JSON type or several. */
  type?: string | string[];
  /** Allowed values. */
  enum?: unknown[];
  /** Object members. */
  properties?: Record<string, Schema>;
  /** Members that must be present. */
  required?: string[];
  /** Schema for members not listed, or whether they are allowed. */
  additionalProperties?: boolean | Schema;
  /** Array element schema. */
  items?: Schema;
  /** Exactly one must match. */
  oneOf?: Schema[];
  /** At least one must match. */
  anyOf?: Schema[];
  /** All must match. */
  allOf?: Schema[];
  /** Smallest allowed number. */
  minimum?: number;
};

/** One operation's 200 answer. */
type Operation = {
  /** Responses by status. */
  responses?: Record<string, { content?: Record<string, { schema?: Schema }> }>;
};

/** The parts of the document the checker reads. */
type Document = {
  /** Operations by path template and method. */
  paths: Record<string, Record<string, Operation>>;
  /** Named schemas. */
  components: { schemas: Record<string, Schema> };
};

/** The daemon's committed OpenAPI 3.1 document. */
const DOC = JSON.parse(
  readFileSync(
    path.join(import.meta.dirname, "..", "..", "..", "crates", "bookclerkd", "openapi.json"),
    "utf8",
  ),
) as Document;

/** Path templates as matchers, longest literal prefix first. */
const ROUTES = Object.keys(DOC.paths).map((template) => ({
  template,
  pattern: new RegExp(
    `^${template.replace(/[.*+?^$()|[\]\\]/g, "\\$&").replace(/\{[^}]+\}/g, "[^/]+")}$`,
  ),
  literal: template.replace(/\{[^}]+\}/g, "").length,
}));
ROUTES.sort((a, b) => b.literal - a.literal);

/**
 * Finds the documented JSON schema of an answer with a given status.
 *
 * @param method - HTTP method.
 * @param pathname - Request path, such as `/api/library/works`.
 * @param status - The answer's HTTP status.
 * @returns The path template and the schema of its JSON answer (null when
 *   the operation documents that status without JSON), or null when no
 *   documented operation matches. `schema` is undefined when the operation
 *   documents neither the status, its range (`4XX`) nor a default.
 */
export function documented(
  method: string,
  pathname: string,
  status: number,
): { template: string; schema: Schema | null | undefined } | null {
  for (const route of ROUTES) {
    if (!route.pattern.test(pathname)) continue;
    const op = DOC.paths[route.template]?.[method.toLowerCase()];
    if (!op) continue;
    const responses = op.responses ?? {};
    const response =
      responses[String(status)] ?? responses[`${String(status)[0]}XX`] ?? responses.default;
    const schema = response ? (response.content?.["application/json"]?.schema ?? null) : undefined;
    return { template: route.template, schema };
  }
  return null;
}

/**
 * The JSON type name of a value, as JSON Schema names it.
 *
 * @param value - Any JSON value.
 * @returns `null`, `array`, `integer`, `number`, `string`, `boolean` or `object`.
 */
function typeOf(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  if (typeof value === "number") return Number.isInteger(value) ? "integer" : "number";
  return typeof value;
}

/**
 * Resolves a reference to its named schema.
 *
 * @param schema - A schema that may be a `$ref`.
 * @returns The schema it names, or itself.
 */
function deref(schema: Schema): Schema {
  if (!schema.$ref) return schema;
  return DOC.components.schemas[schema.$ref.split("/").pop() ?? ""] ?? schema;
}

/**
 * Merges the members of object parts into the properties collected so far.
 * A member two parts both define keeps both definitions (as an `allOf`), so
 * every part's constraints still apply.
 *
 * @param into - The properties collected so far, changed in place.
 * @param from - One part's properties.
 */
function addMembers(into: Record<string, Schema>, from: Record<string, Schema> | undefined): void {
  for (const [key, member] of Object.entries(from ?? {})) {
    const existing = into[key];
    into[key] = existing ? { allOf: [existing, member] } : member;
  }
}

/**
 * Whether every part of an `allOf` describes an object by its members, the
 * way the document extends a struct with a flattened one.
 *
 * @param parts - The `allOf` parts.
 * @returns True when each part (or its own `allOf`) lists properties.
 */
function objectParts(parts: Schema[]): boolean {
  return parts.every((raw) => {
    const part = deref(raw);
    return part.allOf ? objectParts(part.allOf) : part.properties !== undefined;
  });
}

/**
 * Merges the parts of an `allOf` into one schema: the document uses it to
 * extend an object with more members (a flattened Rust struct).
 *
 * @param parts - The `allOf` parts, each one an object (see `objectParts`).
 * @returns One object schema with every part's members and requirements.
 */
function merged(parts: Schema[]): Schema {
  const properties: Record<string, Schema> = {};
  const required: string[] = [];
  for (const raw of parts) {
    const part = deref(raw);
    const inner = part.allOf ? merged(part.allOf) : part;
    addMembers(properties, inner.properties);
    required.push(...(inner.required ?? []));
  }
  return { type: "object", properties, required };
}

/**
 * Checks a value against a schema.
 *
 * @param value - The sample answer, or part of it.
 * @param schema - Its schema.
 * @param at - Where in the answer the value sits, for messages.
 * @returns Every mismatch found, empty when the value conforms.
 */
export function mismatches(value: unknown, schema: Schema, at = "$"): string[] {
  if (schema.$ref) {
    const name = schema.$ref.split("/").pop() ?? "";
    const target = DOC.components.schemas[name];
    if (!target) return [`${at}: unknown schema ${schema.$ref}`];
    return mismatches(value, target, at);
  }
  if (schema.allOf) {
    // Object parts are checked together, so a member one part lists is not
    // "not in the document" for another; any other parts each apply alone.
    if (objectParts(schema.allOf)) return mismatches(value, merged(schema.allOf), at);
    return schema.allOf.flatMap((part) => mismatches(value, part, at));
  }
  const alternatives = schema.oneOf ?? schema.anyOf;
  if (alternatives) {
    const results = alternatives.map((s) => mismatches(value, s, at));
    const matched = results.filter((r) => r.length === 0).length;
    if (matched === 0)
      return [`${at}: matches none of ${alternatives.length} alternatives (${results.flat()[0]})`];
    if (schema.oneOf && matched > 1)
      return [`${at}: matches ${matched} of the oneOf alternatives, where exactly one must match`];
    return [];
  }
  if (schema.enum && !schema.enum.includes(value))
    return [`${at}: ${JSON.stringify(value)} is not one of ${JSON.stringify(schema.enum)}`];
  const actual = typeOf(value);
  if (schema.type !== undefined) {
    const types = Array.isArray(schema.type) ? schema.type : [schema.type];
    const ok = types.includes(actual) || (actual === "integer" && types.includes("number"));
    if (!ok) return [`${at}: ${actual} where the document says ${types.join(" or ")}`];
  }
  if (typeof value === "number" && schema.minimum !== undefined && value < schema.minimum)
    return [`${at}: ${value} is below the minimum ${schema.minimum}`];
  if (Array.isArray(value) && schema.items) {
    const items = schema.items;
    return value.flatMap((v, i) => mismatches(v, items, `${at}[${i}]`));
  }
  if (actual === "object" && (schema.properties || schema.additionalProperties !== undefined)) {
    const object = value as Record<string, unknown>;
    const errors: string[] = [];
    for (const key of schema.required ?? [])
      if (!(key in object)) errors.push(`${at}.${key}: missing`);
    for (const [key, v] of Object.entries(object)) {
      const member = schema.properties?.[key];
      if (member) errors.push(...mismatches(v, member, `${at}.${key}`));
      else if (typeof schema.additionalProperties === "object")
        errors.push(...mismatches(v, schema.additionalProperties, `${at}.${key}`));
      else if (
        schema.additionalProperties === false ||
        (schema.properties && schema.additionalProperties !== true)
      )
        errors.push(`${at}.${key}: not in the document`);
    }
    return errors;
  }
  return [];
}
