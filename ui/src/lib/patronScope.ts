/**
 * Editing patron scopes: the rules that decide which part of the library a
 * Member sees.
 *
 * @module
 */
import type { PatronScope, RuleEffect, ScopeField, ScopeRule } from "@/lib/api";

/** Most rules one scope holds (the server's bound). */
export const MAX_SCOPE_RULES = 16;

/** Longest rule value in UTF-8 bytes (the server's bound). */
export const MAX_SCOPE_VALUE_BYTES = 256;

/** Largest scope as compact JSON in UTF-8 bytes (the server's bound). */
export const MAX_SCOPE_JSON_BYTES = 1024;

/** The fields a rule can match, in menu order, with their labels. */
export const SCOPE_FIELDS: readonly { value: ScopeField; label: string }[] = [
  { value: "genre", label: "Genre" },
  { value: "series", label: "Series" },
  { value: "author", label: "Author" },
  { value: "narrator", label: "Narrator" },
];

/** The rule effects, in menu order, with their labels. */
export const RULE_EFFECTS: readonly { value: RuleEffect; label: string }[] = [
  { value: "include", label: "Show only" },
  { value: "exclude", label: "Hide" },
];

/**
 * A blank rule to fill in.
 *
 * @returns An include rule on genre with no value.
 */
export function newRule(): ScopeRule {
  return { field: "genre", value: "", effect: "include" };
}

/**
 * Why a scope would be refused, or `null` when it is fine. Mirrors the
 * server's checks so the editor can say so before saving.
 *
 * @param scope - The scope being edited.
 * @returns A sentence for the person editing, or `null`.
 */
export function scopeProblem(scope: PatronScope): string | null {
  if (scope.rules.length > MAX_SCOPE_RULES) {
    return `A scope holds at most ${MAX_SCOPE_RULES} rules.`;
  }
  const seen = new Set<string>();
  for (const rule of scope.rules) {
    const value = rule.value.trim();
    if (!value) return "Every rule needs a value.";
    if (new TextEncoder().encode(value).length > MAX_SCOPE_VALUE_BYTES) {
      return `A rule value is at most ${MAX_SCOPE_VALUE_BYTES} bytes.`;
    }
    const key = `${rule.field}\u0000${value.toLowerCase()}\u0000${rule.effect}`;
    if (seen.has(key)) return `The rule "${value}" is there twice.`;
    seen.add(key);
  }
  const trimmed = {
    rules: scope.rules.map((r) => ({ field: r.field, value: r.value.trim(), effect: r.effect })),
  };
  if (new TextEncoder().encode(JSON.stringify(trimmed)).length > MAX_SCOPE_JSON_BYTES) {
    return "These rules are too long together. Use fewer or shorter values.";
  }
  return null;
}

/**
 * A one-line description of a scope for a summary.
 *
 * @param scope - The scope.
 * @returns "Whole library", or how many rules of each kind it has.
 */
export function scopeSummary(scope: PatronScope): string {
  if (scope.rules.length === 0) return "Whole library";
  const include = scope.rules.filter((r) => r.effect === "include").length;
  const exclude = scope.rules.length - include;
  const parts: string[] = [];
  if (include) parts.push(`${include} show-only ${include === 1 ? "rule" : "rules"}`);
  if (exclude) parts.push(`${exclude} hide ${exclude === 1 ? "rule" : "rules"}`);
  return parts.join(", ");
}
