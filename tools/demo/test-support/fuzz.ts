// Seeded mutation helpers for the demo fuzz tests: a
// deterministic PRNG so a failure replays from its seed, and a text mutator
// that splices in fragments chosen by each test, plus seed corpus loading.

/**
 * Every seed file under ../corpus, by path relative to this module
 * (`../corpus/<target>/<name>`). Vite reads them when it transforms this
 * module, so the tests replay them inside the Workers runtime, which has no
 * view of the host filesystem.
 */
const CORPUS: Readonly<Record<string, string>> = import.meta.glob("../corpus/*/*", {
  query: "?raw",
  import: "default",
  eager: true,
});

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
 * The seed corpus of one fuzz target, as `[file name, text]` sorted by name.
 *
 * @param target - Directory under `corpus/`, such as `route`.
 * @returns Seeds.
 * @throws {Error} When the target has no seeds, which means a typo.
 */
export function seeds(target: string): [string, string][] {
  const prefix = `../corpus/${target}/`;
  const out = Object.entries(CORPUS)
    .filter(([path]) => path.startsWith(prefix))
    .map(([path, text]): [string, string] => [path.slice(prefix.length), text])
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  if (out.length === 0) throw new Error(`no seeds in corpus/${target}`);
  return out;
}
