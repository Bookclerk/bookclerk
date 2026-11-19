/**
 * Whether the signed-in patron may see prices and store links, as the
 * session check last reported it (`see_prices` on `GET /api/auth/me`).
 *
 * The library drops prices and store links from its own answers for a
 * patron whose "See prices and store links" member control is off; this
 * keeps the app from building store links of its own for them too.
 */
let visible = true;

/**
 * Records what the latest session check said.
 *
 * @param value - The session's `see_prices`.
 */
export function setPricesVisible(value: boolean): void {
  visible = value;
}

/**
 * Whether prices and store links may be shown.
 *
 * @returns False when the patron's member controls turn them off.
 */
export function pricesVisible(): boolean {
  return visible;
}
