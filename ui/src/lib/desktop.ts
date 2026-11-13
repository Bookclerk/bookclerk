/**
 * The desktop window: the same app, served to a Tauri window over the
 * `bookclerk` custom scheme by a host running inside the window's own
 * process (no listener).
 *
 * In the window, "Join a library" is the first choice: a person signs in to
 * a library they are a member of, and this computer joins it as a **window**
 * (it shows their books and downloads nothing until they turn downloads on,
 * which makes it a branch). Hosting a library here is the alternative.
 */

/** The page origins the desktop window serves the app from. */
export const DESKTOP_ORIGINS: readonly string[] = [
  "bookclerk://localhost",
  "http://bookclerk.localhost",
];

/**
 * Whether the app is running in the desktop window.
 *
 * @param location - The page's protocol and host (defaults to the page's own).
 * @returns True on the desktop window's custom scheme (or its WebView2 form).
 */
export function isDesktopWindow(
  location: { protocol: string; host: string } = window.location,
): boolean {
  return DESKTOP_ORIGINS.includes(`${location.protocol}//${location.host}`);
}
