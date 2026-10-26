// Seeded mutation helpers shared by the dashboard fuzz tests: a
// deterministic PRNG so a failure replays from its seed, and a text mutator
// that splices in fragments chosen by each test, plus seed corpus loading.

import { readdirSync, readFileSync } from "node:fs";

/**
 * Deterministic PRNG (mulberry32).
 *
 * @param seed - 32-bit seed.
 * @returns Uniform values in [0, 1).
 */
export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * Applies one to six random edits: insert a fragment, delete a short run,
 * or insert a random BMP character.
 *
 * @param rand - PRNG.
 * @param text - Input.
 * @param fragments - Fragments to splice in.
 * @returns Mutated text.
 */
export function mutate(rand: () => number, text: string, fragments: string[]): string {
  const pick = (n: number): number => Math.floor(rand() * n);
  for (let k = 1 + pick(6); k > 0; k--) {
    const pos = pick(text.length + 1);
    const op = rand();
    if (op < 0.5) {
      text = text.slice(0, pos) + fragments[pick(fragments.length)] + text.slice(pos);
    } else if (op < 0.75) {
      text = text.slice(0, pos) + text.slice(pos + 1 + pick(8));
    } else {
      text = text.slice(0, pos) + String.fromCharCode(pick(0x3000)) + text.slice(pos);
    }
  }
  return text;
}

/**
 * One item of a list, chosen with one draw from the PRNG.
 *
 * @param rand - PRNG.
 * @param list - Non-empty list.
 * @returns The chosen item.
 * @throws {Error} For an empty list.
 */
export function choose<T>(rand: () => number, list: readonly T[]): T {
  const item = list[Math.floor(rand() * list.length)];
  if (item === undefined) throw new Error("choose from an empty list");
  return item;
}

/**
 * The seed corpus in a directory, as `[file name, text]` sorted by name.
 *
 * @param dir - Corpus directory URL (with a trailing slash).
 * @returns Seeds.
 */
export function seedsIn(dir: URL): [string, string][] {
  return readdirSync(dir)
    .sort()
    .map((name): [string, string] => [name, readFileSync(new URL(name, dir), "utf8")]);
}
