/**
 * Profile-picture resolution: upload > last-used SSO > Gravatar > monogram.
 */

/**
 * HTTPS picture supplied by a linked identity provider.
 */
export type SsoPicture = {
  identity_id: number;
  provider: string;
  picture_url: string;
  last_used_at?: string | null;
};

/**
 * Avatar fields returned on `/me` and `GET /api/users`.
 */
export type AvatarFields = {
  has_avatar?: boolean;
  avatar_source?: string | null;
  gravatar_hash?: string | null;
  sso_pictures?: SsoPicture[];
};

/**
 * Display choice after applying `avatar_source` or auto-resolve.
 */
export type ResolvedAvatar =
  | { kind: "monogram" }
  | { kind: "upload" }
  | { kind: "gravatar"; src: string }
  | { kind: "sso"; src: string; identityId: number };

/**
 * Gravatar image URL that 404s when the email has no Gravatar.
 *
 * @param hash - SHA-256 hex of the trimmed lowercase email.
 * @param size - Pixel size requested from Gravatar.
 * @returns Image URL.
 */
export function gravatarUrl(hash: string, size = 128): string {
  return `https://www.gravatar.com/avatar/${hash}?d=404&s=${size}`;
}

/**
 * SSO picture whose portal session was used most recently.
 *
 * @param pictures - IdP pictures for the user.
 * @returns Last-used picture, or undefined when none exist.
 */
export function lastUsedSso(
  pictures: SsoPicture[] | undefined,
): SsoPicture | undefined {
  const pics = (pictures ?? []).filter((p) => p.picture_url.trim());
  if (pics.length === 0) return undefined;
  return [...pics].sort((a, b) => {
    const ta = a.last_used_at ? Date.parse(a.last_used_at) : 0;
    const tb = b.last_used_at ? Date.parse(b.last_used_at) : 0;
    if (tb !== ta) return tb - ta;
    return b.identity_id - a.identity_id;
  })[0];
}

/**
 * Resolves which picture to show for a user row.
 *
 * Explicit `avatar_source` wins when that option is still available; otherwise
 * auto-resolve uses upload, then last-used SSO, then Gravatar, then monogram.
 *
 * @param fields - Avatar metadata from the daemon.
 * @returns Display kind and image URL when not a monogram.
 */
export function resolveAvatar(fields: AvatarFields): ResolvedAvatar {
  const source = (fields.avatar_source ?? "auto").trim() || "auto";
  if (source !== "auto") {
    const chosen = choiceFor(source, fields);
    if (chosen) return chosen;
  }
  return autoChoice(fields);
}

/**
 * Operator-facing label for an identity-broker id.
 *
 * @param provider - `oidc:google`, `github`, …
 * @returns Short display name.
 */
export function ssoProviderLabel(provider: string): string {
  const id = provider.replace(/^oidc:/i, "").toLowerCase();
  switch (id) {
    case "google":
      return "Google";
    case "github":
      return "GitHub";
    case "discord":
      return "Discord";
    case "apple":
      return "Apple";
    default:
      return id || provider;
  }
}

/**
 * `avatar_source` value matching a resolved display choice.
 *
 * @param resolved - Result of {@link resolveAvatar}.
 * @returns Wire source string.
 */
export function selectedSourceKey(resolved: ResolvedAvatar): string {
  switch (resolved.kind) {
    case "monogram":
      return "monogram";
    case "upload":
      return "upload";
    case "gravatar":
      return "gravatar";
    case "sso":
      return `sso:${resolved.identityId}`;
  }
}

function autoChoice(fields: AvatarFields): ResolvedAvatar {
  if (fields.has_avatar) return { kind: "upload" };
  const sso = lastUsedSso(fields.sso_pictures);
  if (sso) {
    return { kind: "sso", src: sso.picture_url, identityId: sso.identity_id };
  }
  if (fields.gravatar_hash) {
    return { kind: "gravatar", src: gravatarUrl(fields.gravatar_hash) };
  }
  return { kind: "monogram" };
}

function choiceFor(
  source: string,
  fields: AvatarFields,
): ResolvedAvatar | null {
  if (source === "monogram") return { kind: "monogram" };
  if (source === "upload") {
    return fields.has_avatar ? { kind: "upload" } : null;
  }
  if (source === "gravatar" && fields.gravatar_hash) {
    return { kind: "gravatar", src: gravatarUrl(fields.gravatar_hash) };
  }
  if (source.startsWith("sso:")) {
    const id = Number(source.slice(4));
    const pic = fields.sso_pictures?.find((p) => p.identity_id === id);
    if (pic?.picture_url) {
      return { kind: "sso", src: pic.picture_url, identityId: id };
    }
  }
  return null;
}
