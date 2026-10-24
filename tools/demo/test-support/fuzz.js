// Seeded mutation helpers for the demo fuzz tests: a
// deterministic PRNG so a failure replays from its seed, and a text mutator
// that splices in fragments chosen by each test.

/**
 * Deterministic PRNG (mulberry32).
 *
 * @param {number} seed 32-bit seed.
 * @returns {() => number} Uniform values in [0, 1).
 */
export function rng(seed) {
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
 * @param {() => number} rand PRNG.
 * @param {string} text Input.
 * @param {string[]} fragments Fragments to splice in.
 * @returns {string} Mutated text.
 */
export function mutate(rand, text, fragments) {
  const pick = (n) => Math.floor(rand() * n);
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
