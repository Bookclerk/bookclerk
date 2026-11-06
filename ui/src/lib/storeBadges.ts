/**
 * Which store badges a title's details show.
 *
 * A title can name the same store several ways at once: its own `source`,
 * its `sources`, its store editions and the live purchase hints. Each store
 * gets one badge however many of those name it, in any letter case.
 */

/** Where a title's store names come from. */
export type StoreBadgeInputs = {
  /** The title's own store (`detail.source`), shown first. */
  primary?: string | null;
  /** Stores the title is known on (`detail.sources`). */
  sources: readonly string[];
  /** Stores of its catalog editions. */
  editions: readonly string[];
  /** Stores of the purchase hints shown, when commerce is on. */
  hints: readonly string[];
  /** Whether an Audible ASIN is known, which adds Audible. */
  audible: boolean;
};

/**
 * The stores to badge, lowercased, each once, the primary store first and
 * the rest in the order they are first named.
 *
 * @param inputs - Every place the title names a store.
 * @returns Store ids, one per badge.
 */
export function storeBadgeSources(inputs: StoreBadgeInputs): string[] {
  const seen = new Set<string>();
  const add = (name: string | null | undefined) => {
    const id = name?.trim().toLowerCase();
    if (id) seen.add(id);
  };
  add(inputs.primary);
  for (const name of inputs.sources) add(name);
  for (const name of inputs.editions) add(name);
  for (const name of inputs.hints) add(name);
  if (inputs.audible) add("audible");
  return [...seen];
}
