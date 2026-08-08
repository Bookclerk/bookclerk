/** Document paths the React app owns (served as `index.html` by bookclerkd). */
const APP_PATHS = new Set(["/"]);

/** True when `pathname` is a known GUI route (not an API/static/unknown URL). */
export function isAppPath(pathname: string): boolean {
  const raw = pathname || "/";
  const normalized =
    raw.length > 1 && raw.endsWith("/") ? raw.slice(0, -1) : raw;
  return APP_PATHS.has(normalized === "" ? "/" : normalized);
}
