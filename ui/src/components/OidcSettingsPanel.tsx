import { useEffect, useState, type FormEvent } from "react";
import { ChevronDown, Plus, Trash2 } from "lucide-react";
import { SsoProviderMark } from "@/components/SsoProviderMark";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  createOidcClient,
  deleteOidcClient,
  fetchMfaPolicy,
  fetchOidcConfig,
  isApiError,
  listOidcClients,
  putMfaPolicy,
  putOidcConfig,
  rotateOidcClientSecret,
  updateOidcClient,
  type OidcAsClient,
  type OidcBrokerConfigView,
  type OidcProvisionMode,
  type OidcProviderConfigUpdate,
  type OidcSecretSource,
} from "@/lib/api";

const selectClassName =
  "h-9 w-full rounded-md border border-ink/15 bg-card-strong px-2 text-sm text-ink outline-none focus:border-teal";

const PRESETS = [
  ["", "Custom OIDC"],
  ["google", "Google"],
  ["github", "GitHub"],
  ["apple", "Apple"],
  ["discord", "Discord"],
] as const;

const SOCIAL_PRESETS: Record<
  string,
  { id: string; name: string; scopes: string[] }
> = {
  google: { id: "google", name: "Google", scopes: ["openid", "profile", "email"] },
  github: { id: "github", name: "GitHub", scopes: ["read:user", "user:email"] },
  apple: { id: "apple", name: "Apple", scopes: ["openid", "profile", "email"] },
  discord: { id: "discord", name: "Discord", scopes: ["identify", "email"] },
};

const SOCIAL_PROVISIONS: [OidcProvisionMode, string][] = [
  ["allowlist", "Allowlisted emails or domains"],
  ["any", "Anyone with an account"],
  ["invite_only", "Invite only (no new accounts)"],
];

const ENTERPRISE_PROVISIONS: [OidcProvisionMode, string][] = [
  ["mapped_role", "Mapped role (enterprise groups)"],
  ...SOCIAL_PROVISIONS,
];

function canonicalSocialPreset(preset: string | null | undefined): string | null {
  const trimmed = (preset ?? "").trim();
  if (!trimmed) {
    return null;
  }
  const match = PRESETS.find(
    ([value]) => value && value.toLowerCase() === trimmed.toLowerCase(),
  );
  return match ? match[0] : trimmed;
}

type DraftProvider = OidcProviderConfigUpdate & {
  has_client_secret: boolean;
  has_apple_private_key: boolean;
  secret_source: OidcSecretSource;
  clearSecret: boolean;
  clearAppleKey: boolean;
  roleMapText: string;
  startsOpen: boolean;
};

function csv(values: string[]): string {
  return values.join(", ");
}

function splitCsv(raw: string): string[] {
  return raw
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function roleMapToText(map: Record<string, string>): string {
  return Object.entries(map)
    .map(([claim, role]) => `${claim} = ${role}`)
    .join("\n");
}

function parseRoleMap(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const sep = trimmed.includes("=") ? "=" : trimmed.includes(":") ? ":" : null;
    if (!sep) continue;
    const idx = trimmed.indexOf(sep);
    const claim = trimmed.slice(0, idx).trim();
    const role = trimmed.slice(idx + 1).trim();
    if (claim && role) out[claim] = role;
  }
  return out;
}

function defaultLinkByEmail(preset: string | null | undefined): boolean {
  return Boolean(canonicalSocialPreset(preset));
}

function toDraft(view: OidcBrokerConfigView): {
  enabled: boolean;
  domains: string;
  providers: DraftProvider[];
} {
  return {
    enabled: view.enabled,
    domains: csv(view.allowed_email_domains),
    providers: view.providers.map((p) => ({
      id: p.id,
      name: p.name,
      preset: canonicalSocialPreset(p.preset) ?? "",
      issuer: p.issuer ?? "",
      client_id: p.client_id,
      client_secret: "",
      scopes: p.scopes,
      provision: p.provision,
      default_role: p.default_role || "member",
      role_claim: p.role_claim || "groups",
      role_map: p.role_map ?? {},
      link_by_email: p.link_by_email,
      allowed_email_domains: p.allowed_email_domains ?? [],
      allowed_emails: p.allowed_emails ?? [],
      allowed_subjects: p.allowed_subjects ?? [],
      apple_team_id: p.apple_team_id ?? "",
      apple_key_id: p.apple_key_id ?? "",
      apple_private_key: "",
      has_client_secret: p.has_client_secret,
      has_apple_private_key: Boolean(p.has_apple_private_key),
      secret_source: p.secret_source,
      clearSecret: false,
      clearAppleKey: false,
      roleMapText: roleMapToText(p.role_map ?? {}),
      startsOpen: false,
    })),
  };
}

/** Draft card for Add provider: Google allowlist, the usual homelab social setup. */
function emptyProvider(): DraftProvider {
  const google = SOCIAL_PRESETS.google;
  return {
    id: google.id,
    name: google.name,
    preset: "google",
    issuer: "",
    client_id: "",
    client_secret: "",
    scopes: google.scopes,
    provision: "allowlist",
    default_role: "member",
    role_claim: "groups",
    role_map: {},
    link_by_email: defaultLinkByEmail("google"),
    allowed_email_domains: [],
    allowed_emails: [],
    allowed_subjects: [],
    apple_team_id: "",
    apple_key_id: "",
    apple_private_key: "",
    has_client_secret: false,
    has_apple_private_key: false,
    secret_source: "none",
    clearSecret: false,
    clearAppleKey: false,
    roleMapText: "",
    startsOpen: true,
  };
}

/** Fills id, name, and scopes when switching to a built-in social preset. */
function applyPreset(provider: DraftProvider, preset: string): DraftProvider {
  const next: DraftProvider = { ...provider, preset };
  const prevMeta = SOCIAL_PRESETS[canonicalSocialPreset(provider.preset) ?? ""];
  const meta = SOCIAL_PRESETS[preset];
  next.link_by_email = defaultLinkByEmail(preset);
  if (!meta) {
    return next;
  }
  if (!next.id.trim() || (prevMeta && next.id.trim() === prevMeta.id)) {
    next.id = meta.id;
  }
  if (!next.name.trim() || (prevMeta && next.name.trim() === prevMeta.name)) {
    next.name = meta.name;
  }
  next.issuer = "";
  next.scopes = meta.scopes;
  if (next.provision === "mapped_role" && !next.roleMapText.trim()) {
    next.provision = "allowlist";
  }
  return next;
}

/** True when this provider maps IdP groups instead of a flat allowlist. */
function providerUsesMappedRole(provider: DraftProvider): boolean {
  return provider.provision === "mapped_role" || Boolean(provider.roleMapText.trim());
}

/** Open Advanced when a social card already has non-default policy or a custom id. */
function socialAdvancedOpen(provider: DraftProvider): boolean {
  const preset = canonicalSocialPreset(provider.preset);
  const meta = preset ? SOCIAL_PRESETS[preset] : undefined;
  return (
    providerUsesMappedRole(provider) ||
    provider.allowed_subjects.length > 0 ||
    Boolean(meta && provider.id.trim() && provider.id.trim() !== meta.id)
  );
}

function secretHint(source: OidcSecretSource, has: boolean): string {
  if (!has) return "Optional. Prefer env or the sealed store over config.toml.";
  switch (source) {
    case "env":
      return "A BOOKCLERK_OIDC_*_CLIENT_SECRET env var is set (wins at runtime). Leave blank to keep it.";
    case "config":
      return "A secret is present in config.toml. Saving migrates it to the sealed store. Leave blank to keep it.";
    case "store":
      return "A secret is stored in encrypted_secrets. Leave blank to keep it.";
    default:
      return "Optional. Prefer env or the sealed store over config.toml.";
  }
}

const fieldLabelClass = "flex flex-col gap-1 text-sm font-medium text-ink";
const textareaClassName =
  "min-h-[4.5rem] w-full rounded-md border border-ink/15 bg-card-strong px-3 py-2 text-sm text-ink shadow-sm placeholder:text-ink/40 focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

const LOOPBACK_ISSUER = "http://localhost:8787";

function pageOrigin(): string {
  if (typeof window === "undefined") return LOOPBACK_ISSUER;
  return window.location.origin.replace(/\/+$/, "");
}

/** Rewrite loopback IPs so Settings previews match WebAuthn / tray URLs. */
function rewriteLoopbackOrigin(origin: string): string {
  const trimmed = trimOrigin(origin);
  try {
    const url = new URL(trimmed);
    if (url.hostname === "127.0.0.1" || url.hostname === "[::1]" || url.hostname === "::1") {
      url.hostname = "localhost";
      return url.origin;
    }
  } catch {
    return trimmed;
  }
  return trimmed;
}

/** Effective issuer: typed pin, else API-detected origin, else this page. */
function previewIssuer(publicOrigin: string, detectedOrigin: string): string {
  const typed = rewriteLoopbackOrigin(publicOrigin);
  if (typed) return typed;
  return rewriteLoopbackOrigin(detectedOrigin || pageOrigin());
}

function providerStatus(provider: DraftProvider, preset: string | null): string {
  if (preset === "apple") {
    if (!provider.client_id.trim()) return "Missing client ID";
    if (!provider.has_apple_private_key && !provider.apple_private_key?.trim()) {
      return "Missing Apple key";
    }
  } else if (!provider.client_id.trim()) {
    return "Missing client ID";
  } else if (!provider.has_client_secret && !provider.client_secret?.trim()) {
    return "Missing secret";
  }
  switch (provider.provision) {
    case "allowlist":
      return "Allowlisted emails or domains";
    case "any":
      return "Anyone with an account";
    case "invite_only":
      return "Invite only";
    case "mapped_role":
      return "Mapped role";
    default:
      return "Configured";
  }
}

function providerCapabilities(preset: string | null): string {
  if (preset === "apple") {
    return "This provider can send: verified email; name on first consent only; no profile photo.";
  }
  if (preset === "google" || preset === "github" || preset === "discord") {
    return "This provider can send: verified email, name, and profile photo.";
  }
  return "This provider can send whatever your IdP includes in profile and email scopes.";
}

/** Trimmed origin with no trailing slash, or empty when unset. */
function trimOrigin(origin: string): string {
  return origin.trim().replace(/\/+$/, "");
}

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/** Label plus copyable URL for Bookclerk-as-IdP discovery endpoints. */
function CopyableValue({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);

  async function onCopy() {
    const ok = await copyText(value);
    if (!ok) return;
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  }

  return (
    <div className="flex flex-col gap-1">
      <p className="text-xs font-medium text-ink/70">{label}</p>
      <div className="flex items-center gap-2 rounded-md border border-ink/15 bg-card-strong px-3 py-2">
        <code className="min-w-0 flex-1 break-all text-xs text-ink/80">{value}</code>
        <Button
          type="button"
          variant="secondary"
          className="h-8 shrink-0"
          onClick={() => void onCopy()}
        >
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
    </div>
  );
}

/**
 * One identity-broker provider card. Social presets show credentials and access
 * policy; issuer, scopes, and group mapping stay under Advanced.
 */
function ProviderCard({
  provider,
  onChange,
  onRemove,
}: {
  provider: DraftProvider;
  onChange: (next: DraftProvider) => void;
  onRemove: () => void;
}) {
  const preset = canonicalSocialPreset(provider.preset);
  const isSocial = Boolean(preset);
  const meta = preset ? SOCIAL_PRESETS[preset] : undefined;
  const showAllowlist =
    provider.provision === "allowlist" ||
    (provider.provision === "any" &&
      (provider.allowed_email_domains.length > 0 || provider.allowed_emails.length > 0));
  const showDefaultRole = provider.provision !== "invite_only";
  const provisionOptions =
    isSocial && !providerUsesMappedRole(provider)
      ? SOCIAL_PROVISIONS
      : ENTERPRISE_PROVISIONS;
  const title =
    provider.name.trim() ||
    provider.id.trim() ||
    meta?.name ||
    "New provider";
  const [expanded, setExpanded] = useState(provider.startsOpen);
  const [advancedOpen, setAdvancedOpen] = useState(
    () => isSocial && socialAdvancedOpen(provider),
  );
  const status = providerStatus(provider, preset);

  function patch(next: Partial<DraftProvider>) {
    onChange({ ...provider, ...next });
  }

  return (
    <li className="flex flex-col gap-4 rounded-md border border-ink/10 bg-card p-4">
      <div className="flex items-start gap-2">
        <button
          type="button"
          className="flex min-w-0 flex-1 items-start gap-3 text-left"
          onClick={() => setExpanded((cur) => !cur)}
          aria-expanded={expanded}
        >
          <SsoProviderMark preset={preset} title={title} className="mt-0.5" />
          <span className="min-w-0 flex-1">
            <span className="block text-sm font-semibold text-ink">{title}</span>
            <span className="mt-0.5 block text-xs text-ink/50">{status}</span>
          </span>
          <ChevronDown
            className={
              expanded
                ? "mt-0.5 h-4 w-4 shrink-0 rotate-180 text-ink/50 transition-transform"
                : "mt-0.5 h-4 w-4 shrink-0 text-ink/50 transition-transform"
            }
          />
        </button>
        <Button type="button" variant="ghost" onClick={onRemove} aria-label="Remove provider">
          <Trash2 className="h-4 w-4" />
        </Button>
      </div>
      <p className="text-xs text-ink/55">{providerCapabilities(preset)}</p>
      <div className={expanded ? "grid gap-3 sm:grid-cols-2" : "hidden"}>
        <label className={fieldLabelClass}>
          Preset
          <select
            className={selectClassName}
            value={provider.preset ?? ""}
            onChange={(e) => onChange(applyPreset(provider, e.target.value))}
          >
            {PRESETS.map(([value, label]) => (
              <option key={value || "custom"} value={value}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <label className={fieldLabelClass}>
          Display name
          <Input
            value={provider.name}
            onChange={(e) => patch({ name: e.target.value })}
            placeholder={meta?.name || "Company SSO"}
          />
        </label>
        {!isSocial ? (
          <>
            <label className={fieldLabelClass}>
              Id
              <Input
                value={provider.id}
                onChange={(e) => patch({ id: e.target.value })}
                placeholder="corp"
                required
              />
            </label>
            <label className={fieldLabelClass}>
              Issuer URL
              <Input
                value={provider.issuer ?? ""}
                onChange={(e) => patch({ issuer: e.target.value })}
                placeholder="https://idp.example.com/realms/corp"
                required
              />
            </label>
          </>
        ) : null}
        <label className={fieldLabelClass}>
          Client ID
          <Input
            value={provider.client_id}
            onChange={(e) => patch({ client_id: e.target.value })}
            required
          />
        </label>
        {preset !== "apple" ? (
          <label className={fieldLabelClass}>
            Client secret
            <Input
              type="password"
              autoComplete="new-password"
              value={provider.client_secret ?? ""}
              onChange={(e) =>
                patch({
                  client_secret: e.target.value,
                  clearSecret: false,
                })
              }
              placeholder={provider.has_client_secret ? "unchanged" : "optional"}
              disabled={provider.clearSecret}
            />
            <span className="text-xs font-normal text-ink/50">
              {secretHint(provider.secret_source, provider.has_client_secret)}
            </span>
            {provider.has_client_secret ? (
              <label className="flex items-center gap-2 text-xs font-normal text-ink/70">
                <input
                  type="checkbox"
                  checked={provider.clearSecret}
                  onChange={(e) =>
                    patch({
                      clearSecret: e.target.checked,
                      client_secret: e.target.checked ? "" : provider.client_secret,
                    })
                  }
                />
                Clear stored secret
              </label>
            ) : null}
          </label>
        ) : (
          <span className="hidden sm:block" />
        )}
        {preset === "apple" ? (
          <>
            <label className={`${fieldLabelClass} sm:col-span-2`}>
              Client secret
              <span className="text-xs font-normal text-ink/50">
                Apple mints this from your Team ID, Key ID, and .p8 key. A pasted secret is
                optional.
              </span>
              <Input
                type="password"
                autoComplete="new-password"
                value={provider.client_secret ?? ""}
                onChange={(e) =>
                  patch({
                    client_secret: e.target.value,
                    clearSecret: false,
                  })
                }
                placeholder={provider.has_client_secret ? "unchanged" : "usually leave blank"}
                disabled={provider.clearSecret}
              />
            </label>
            <label className={fieldLabelClass}>
              Apple Team ID
              <Input
                value={provider.apple_team_id ?? ""}
                onChange={(e) => patch({ apple_team_id: e.target.value })}
                placeholder="AB12CDEF34"
              />
            </label>
            <label className={fieldLabelClass}>
              Apple Key ID
              <Input
                value={provider.apple_key_id ?? ""}
                onChange={(e) => patch({ apple_key_id: e.target.value })}
                placeholder="WXYZ123456"
              />
            </label>
            <label className={`${fieldLabelClass} sm:col-span-2`}>
              Apple private key (.p8)
              <Input
                type="password"
                autoComplete="new-password"
                value={provider.apple_private_key ?? ""}
                onChange={(e) =>
                  patch({
                    apple_private_key: e.target.value,
                    clearAppleKey: false,
                  })
                }
                placeholder={
                  provider.has_apple_private_key ? "unchanged" : "PEM private key"
                }
                disabled={provider.clearAppleKey}
              />
              {provider.has_apple_private_key ? (
                <label className="flex items-center gap-2 text-xs font-normal text-ink/70">
                  <input
                    type="checkbox"
                    checked={provider.clearAppleKey}
                    onChange={(e) =>
                      patch({
                        clearAppleKey: e.target.checked,
                        apple_private_key: e.target.checked
                          ? ""
                          : provider.apple_private_key,
                      })
                    }
                  />
                  Clear stored Apple private key
                </label>
              ) : null}
            </label>
          </>
        ) : null}
        <label className={fieldLabelClass}>
          Who can sign in
          <select
            className={selectClassName}
            value={provider.provision}
            onChange={(e) =>
              patch({ provision: e.target.value as OidcProvisionMode })
            }
          >
            {provisionOptions.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </label>
        {showDefaultRole ? (
          <label className={fieldLabelClass}>
            Default role
            <select
              className={selectClassName}
              value={provider.default_role}
              onChange={(e) => patch({ default_role: e.target.value })}
            >
              <option value="member">Member</option>
              <option value="administrator">Administrator</option>
              <option value="owner">Owner</option>
            </select>
          </label>
        ) : null}
        <label className="flex flex-col gap-1 text-sm font-medium text-ink sm:col-span-2">
          <span className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={provider.link_by_email}
              onChange={(e) => patch({ link_by_email: e.target.checked })}
            />
            Link by verified email
          </span>
          <span className="text-xs font-normal text-ink/50">
            When this provider marks the email verified, attach the login to the existing
            Bookclerk user with that address. On for Google, GitHub, Apple, and Discord;
            leave off for custom issuers that may not verify email.
          </span>
        </label>
        {showAllowlist ? (
          <>
            <label className={fieldLabelClass}>
              Allowed email domains
              <Input
                value={csv(provider.allowed_email_domains)}
                onChange={(e) =>
                  patch({ allowed_email_domains: splitCsv(e.target.value) })
                }
                placeholder="family.example"
              />
            </label>
            <label className={fieldLabelClass}>
              Allowed emails
              <Input
                value={csv(provider.allowed_emails)}
                onChange={(e) => patch({ allowed_emails: splitCsv(e.target.value) })}
                placeholder="you@example.com"
              />
            </label>
          </>
        ) : null}
        {!isSocial && providerUsesMappedRole(provider) ? (
          <>
            <label className={fieldLabelClass}>
              Role claim
              <Input
                value={provider.role_claim}
                onChange={(e) => patch({ role_claim: e.target.value })}
                placeholder="groups"
              />
            </label>
            <label className={`${fieldLabelClass} sm:col-span-2`}>
              Role map
              <textarea
                className={textareaClassName}
                value={provider.roleMapText}
                onChange={(e) => patch({ roleMapText: e.target.value })}
                placeholder={"bookclerk-owners = owner\nbookclerk-admins = administrator"}
              />
              <span className="text-xs font-normal text-ink/50">
                One <code>claim = owner|administrator|member</code> per line. Never{" "}
                <code>operator</code>.
              </span>
            </label>
          </>
        ) : null}
      </div>
      <details
        className={
          expanded
            ? "rounded-md border border-ink/10 bg-card-mid px-3 py-2"
            : "hidden"
        }
        open={advancedOpen}
        onToggle={(e) => setAdvancedOpen(e.currentTarget.open)}
      >
        <summary className="cursor-pointer text-sm font-medium text-ink">Advanced</summary>
        <div className="mt-3 grid gap-3 sm:grid-cols-2">
          {isSocial ? (
            <label className={fieldLabelClass}>
              Id
              <Input
                value={provider.id}
                onChange={(e) => patch({ id: e.target.value })}
                placeholder={meta?.id || "google"}
              />
              <span className="text-xs font-normal text-ink/50">
                Stored as{" "}
                <code>{`oidc:${provider.id.trim() || meta?.id || "id"}`}</code>. Leave as
                the preset name unless you run more than one app of this kind.
              </span>
            </label>
          ) : null}
          <label className={`${fieldLabelClass} ${isSocial ? "" : "sm:col-span-2"}`}>
            Scopes
            <Input
              value={csv(provider.scopes)}
              onChange={(e) => patch({ scopes: splitCsv(e.target.value) })}
            />
          </label>
          {isSocial ? (
            <>
              <label className="flex items-center gap-2 text-sm font-medium text-ink sm:col-span-2">
                <input
                  type="checkbox"
                  checked={provider.provision === "mapped_role"}
                  onChange={(e) =>
                    patch({
                      provision: e.target.checked ? "mapped_role" : "allowlist",
                    })
                  }
                />
                Map IdP groups to Bookclerk roles
              </label>
              {provider.provision === "mapped_role" ? (
                <>
                  <label className={fieldLabelClass}>
                    Role claim
                    <Input
                      value={provider.role_claim}
                      onChange={(e) => patch({ role_claim: e.target.value })}
                      placeholder="groups"
                    />
                  </label>
                  <label className={`${fieldLabelClass} sm:col-span-2`}>
                    Role map
                    <textarea
                      className={textareaClassName}
                      value={provider.roleMapText}
                      onChange={(e) => patch({ roleMapText: e.target.value })}
                      placeholder={"bookclerk-owners = owner\nbookclerk-admins = administrator"}
                    />
                  </label>
                </>
              ) : null}
            </>
          ) : null}
          <label className={`${fieldLabelClass} sm:col-span-2`}>
            Allowed subjects
            <Input
              value={csv(provider.allowed_subjects)}
              onChange={(e) => patch({ allowed_subjects: splitCsv(e.target.value) })}
            />
          </label>
        </div>
      </details>
    </li>
  );
}

type DraftClient = {
  originalId: string;
  client_id: string;
  name: string;
  redirectText: string;
  confidential: boolean;
  issue_refresh_token: boolean;
  scopeProfile: boolean;
  scopeEmail: boolean;
  has_secret: boolean;
  enabled: boolean;
  plugin_id: string | null;
  persisted: boolean;
  startsOpen: boolean;
  plaintextSecret: string | null;
};

function clientFromApi(row: OidcAsClient): DraftClient {
  const scopes = row.allowed_scopes.map((s) => s.toLowerCase());
  return {
    originalId: row.client_id,
    client_id: row.client_id,
    name: row.name ?? "",
    redirectText: (row.redirect_uris ?? []).join("\n"),
    confidential: row.confidential || row.has_secret,
    issue_refresh_token: row.issue_refresh_token,
    scopeProfile: scopes.includes("profile"),
    scopeEmail: scopes.includes("email"),
    has_secret: row.has_secret,
    enabled: row.enabled !== false,
    plugin_id: row.plugin_id?.trim() || null,
    persisted: true,
    startsOpen: false,
    plaintextSecret: row.client_secret ?? null,
  };
}

function emptyClient(): DraftClient {
  return {
    originalId: "",
    client_id: "",
    name: "",
    redirectText: "",
    confidential: false,
    issue_refresh_token: true,
    scopeProfile: true,
    scopeEmail: true,
    has_secret: false,
    enabled: true,
    plugin_id: null,
    persisted: false,
    startsOpen: true,
    plaintextSecret: null,
  };
}

function clientWrite(draft: DraftClient, currentPassword?: string) {
  const scopes = ["openid"];
  if (draft.scopeProfile) scopes.push("profile");
  if (draft.scopeEmail) scopes.push("email");
  return {
    client_id: draft.client_id.trim(),
    name: draft.name.trim() || null,
    redirect_uris: draft.redirectText
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean),
    confidential: draft.confidential,
    issue_refresh_token: draft.issue_refresh_token,
    allowed_scopes: scopes,
    enabled: draft.enabled,
    current_password: currentPassword,
  };
}

function clientStatus(client: DraftClient): string {
  if (!client.client_id.trim()) return "New client";
  const bits: string[] = [];
  bits.push(client.enabled ? "Enabled" : "Disabled");
  if (client.plugin_id) bits.push("Plugin");
  if (client.confidential) {
    bits.push(
      client.has_secret || client.plaintextSecret
        ? "Confidential"
        : "Confidential (secret pending)",
    );
  } else {
    bits.push("Public PKCE");
  }
  return bits.join(" · ");
}

function ClientCard({
  client,
  currentPassword,
  onChange,
  onSaved,
  onRemoved,
  onError,
  onNotice,
}: {
  client: DraftClient;
  currentPassword?: string;
  onChange: (next: DraftClient) => void;
  onSaved: (next: DraftClient) => void;
  onRemoved: () => void;
  onError: (message: string) => void;
  onNotice: (message: string) => void;
}) {
  const [open, setOpen] = useState(client.startsOpen);
  const [busy, setBusy] = useState(false);
  const title = client.name.trim() || client.client_id.trim() || "New client";
  const pluginOwned = Boolean(client.plugin_id);

  function patch(next: Partial<DraftClient>) {
    onChange({ ...client, ...next });
  }

  async function persist(next: DraftClient) {
    const body = clientWrite(next, currentPassword);
    const saved = next.persisted
      ? await updateOidcClient(next.originalId || next.client_id, body)
      : await createOidcClient(body);
    const mapped = clientFromApi(saved);
    mapped.startsOpen = true;
    mapped.plaintextSecret = saved.client_secret ?? null;
    onSaved(mapped);
    return saved;
  }

  async function onSave() {
    setBusy(true);
    try {
      const saved = await persist(client);
      onNotice(
        saved.client_secret
          ? "Client saved. Copy the secret now — it will not be shown again."
          : "OIDC client saved.",
      );
    } catch (err) {
      onError(
        isApiError(err) ? err.message : err instanceof Error ? err.message : "Failed to save client",
      );
    } finally {
      setBusy(false);
    }
  }

  async function onToggleEnabled(enabled: boolean) {
    patch({ enabled });
    if (!client.persisted) return;
    setBusy(true);
    try {
      await persist({ ...client, enabled });
      onNotice(enabled ? "OIDC client enabled." : "OIDC client disabled.");
    } catch (err) {
      patch({ enabled: client.enabled });
      onError(
        isApiError(err)
          ? err.message
          : err instanceof Error
            ? err.message
            : "Failed to update client",
      );
    } finally {
      setBusy(false);
    }
  }

  async function onRotate() {
    if (!client.persisted) return;
    setBusy(true);
    try {
      const saved = await rotateOidcClientSecret(
        client.originalId || client.client_id,
        currentPassword,
      );
      const next = clientFromApi(saved);
      next.startsOpen = true;
      next.plaintextSecret = saved.client_secret ?? null;
      onSaved(next);
      onNotice("New client secret generated. Copy it now — it will not be shown again.");
    } catch (err) {
      onError(
        isApiError(err)
          ? err.message
          : err instanceof Error
            ? err.message
            : "Failed to rotate secret",
      );
    } finally {
      setBusy(false);
    }
  }

  async function onDelete() {
    if (!client.persisted) {
      onRemoved();
      return;
    }
    setBusy(true);
    try {
      await deleteOidcClient(client.originalId || client.client_id, currentPassword);
      onRemoved();
      onNotice("OIDC client deleted.");
    } catch (err) {
      onError(
        isApiError(err)
          ? err.message
          : err instanceof Error
            ? err.message
            : "Failed to delete client",
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <li className="flex flex-col gap-4 rounded-md border border-ink/10 bg-card p-4">
      <div className="flex items-start gap-2">
        <button
          type="button"
          className="flex min-w-0 flex-1 items-start gap-3 text-left"
          onClick={() => setOpen((cur) => !cur)}
          aria-expanded={open}
        >
          <span className="min-w-0 flex-1">
            <span className="block text-sm font-semibold text-ink">{title}</span>
            <span className="mt-0.5 block text-xs text-ink/50">{clientStatus(client)}</span>
          </span>
          <ChevronDown
            className={
              open
                ? "mt-0.5 h-4 w-4 shrink-0 rotate-180 text-ink/50 transition-transform"
                : "mt-0.5 h-4 w-4 shrink-0 text-ink/50 transition-transform"
            }
          />
        </button>
        <label
          className="mt-0.5 flex shrink-0 items-center gap-2 text-sm font-medium text-ink"
          onClick={(e) => e.stopPropagation()}
        >
          <input
            type="checkbox"
            checked={client.enabled}
            disabled={busy}
            onChange={(e) => void onToggleEnabled(e.target.checked)}
          />
          Enabled
        </label>
        {pluginOwned ? null : (
        <Button
          type="button"
          variant="ghost"
          onClick={() => void onDelete()}
          aria-label="Remove client"
          disabled={busy}
        >
          <Trash2 className="h-4 w-4" />
        </Button>
        )}
      </div>
      <div className={open ? "grid gap-3 sm:grid-cols-2" : "hidden"}>
          <label className={fieldLabelClass}>
            Display name
            <Input
              value={client.name}
              onChange={(e) => patch({ name: e.target.value })}
              placeholder="Audiobook player"
            />
          </label>
          <label className={fieldLabelClass}>
            Client id
            <Input
              value={client.client_id}
              onChange={(e) => patch({ client_id: e.target.value })}
              placeholder="player"
              required
              disabled={client.persisted}
            />
          </label>
          <label className={`${fieldLabelClass} sm:col-span-2`}>
            Redirect URIs
            <textarea
              className={
                pluginOwned
                  ? `${textareaClassName} cursor-not-allowed bg-ink/5 text-ink/70`
                  : textareaClassName
              }
              value={client.redirectText}
              onChange={(e) => patch({ redirectText: e.target.value })}
              placeholder={"https://player.example/callback"}
              readOnly={pluginOwned}
            />
            <span className="text-xs font-normal text-ink/50">
              {pluginOwned
                ? "Redirect URIs come from this plugin’s server URL in Settings → Plugins (plus the plugin’s callback path). They cannot be edited here."
                : "One absolute http(s) URL per line. Player callbacks use the player’s own origin and path, not Bookclerk’s listen port."}
            </span>
          </label>
          <label className="flex items-center gap-2 text-sm font-medium text-ink sm:col-span-2">
            <input
              type="checkbox"
              checked={client.confidential}
              onChange={(e) => patch({ confidential: e.target.checked })}
            />
            Confidential client (client secret)
          </label>
          {client.confidential ? (
            <div className="sm:col-span-2 space-y-2">
              {client.plaintextSecret ? (
                <CopyableValue label="Client secret (shown once)" value={client.plaintextSecret} />
              ) : (
                <p className="text-xs text-ink/55">
                  {client.has_secret
                    ? "A secret is stored. Rotate to mint a new one — the previous value cannot be shown."
                    : "Saving will generate a secret and show it once."}
                </p>
              )}
              {client.persisted && client.has_secret ? (
                <Button type="button" variant="secondary" disabled={busy} onClick={() => void onRotate()}>
                  {busy ? "Working…" : "Rotate secret"}
                </Button>
              ) : null}
            </div>
          ) : (
            <p className="text-xs text-ink/55 sm:col-span-2">
              Public PKCE client (token endpoint auth: none). No client secret is issued.
            </p>
          )}
          <label className="flex items-center gap-2 text-sm font-medium text-ink sm:col-span-2">
            <input
              type="checkbox"
              checked={client.issue_refresh_token}
              onChange={(e) => patch({ issue_refresh_token: e.target.checked })}
            />
            Issue refresh tokens
          </label>
          <fieldset className="sm:col-span-2 space-y-1">
            <legend className="text-sm font-medium text-ink">Scopes</legend>
            <p className="text-xs text-ink/50">
              Access and ID tokens are always issued. Honor these scopes on userinfo / ID token
              claims.
            </p>
            <label className="flex items-center gap-2 text-sm text-ink">
              <input type="checkbox" checked disabled />
              openid
            </label>
            <label className="flex items-center gap-2 text-sm text-ink">
              <input
                type="checkbox"
                checked={client.scopeProfile}
                onChange={(e) => patch({ scopeProfile: e.target.checked })}
              />
              profile
            </label>
            <label className="flex items-center gap-2 text-sm text-ink">
              <input
                type="checkbox"
                checked={client.scopeEmail}
                onChange={(e) => patch({ scopeEmail: e.target.checked })}
              />
              email
            </label>
          </fieldset>
          <div className="sm:col-span-2">
            <Button type="button" disabled={busy} onClick={() => void onSave()}>
              {busy ? "Saving…" : client.persisted ? "Save client" : "Create client"}
            </Button>
          </div>
        </div>
    </li>
  );
}

/**
 * Owner/operator Settings tab: SSO into Bookclerk, then Bookclerk as IdP.
 *
 * Empty public origin uses a bound loopback origin (localhost in tray or cargo
 * dev). Production hosts must pin `https://…`.
 */
export function OidcSettingsPanel() {
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [publicOrigin, setPublicOrigin] = useState("");
  const [detectedOrigin, setDetectedOrigin] = useState("");
  const [enabled, setEnabled] = useState(false);
  const [domains, setDomains] = useState("");
  const [providers, setProviders] = useState<DraftProvider[]>([]);
  const [clients, setClients] = useState<DraftClient[]>([]);
  const [currentPassword, setCurrentPassword] = useState("");
  const [requireSecondFactor, setRequireSecondFactor] = useState(false);
  const [savingMfa, setSavingMfa] = useState(false);

  async function reload() {
    const [view, mfa, listed] = await Promise.all([
      fetchOidcConfig(),
      fetchMfaPolicy(),
      listOidcClients(),
    ]);
    const draft = toDraft(view);
    setEnabled(draft.enabled);
    setDomains(draft.domains);
    setProviders(draft.providers);
    setPublicOrigin(view.public_origin?.trim() || "");
    setDetectedOrigin(view.detected_origin?.trim() || pageOrigin());
    setRequireSecondFactor(mfa.require_second_factor);
    setClients(listed.map(clientFromApi));
  }

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      setLoading(true);
      setError(null);
      try {
        await reload();
      } catch (err) {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : "Failed to load sign-in providers");
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  async function onSaveMfa() {
    setSavingMfa(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await putMfaPolicy(
        requireSecondFactor,
        currentPassword.trim() || undefined,
      );
      setRequireSecondFactor(saved.require_second_factor);
      setCurrentPassword("");
      setNotice("Password sign-in policy saved.");
    } catch (err) {
      setError(
        isApiError(err) ? err.message : err instanceof Error ? err.message : "Failed to save",
      );
    } finally {
      setSavingMfa(false);
    }
  }

  async function onSave(event: FormEvent) {
    event.preventDefault();
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const body = {
        enabled,
        allowed_email_domains: splitCsv(domains),
        public_origin: publicOrigin.trim(),
        providers: providers.map((p) => {
          const preset = canonicalSocialPreset(p.preset);
          const meta = preset ? SOCIAL_PRESETS[preset] : undefined;
          const update: OidcProviderConfigUpdate = {
            id: p.id.trim() || meta?.id || "",
            name: p.name.trim() || meta?.name || p.id.trim(),
            preset,
            issuer: p.issuer?.trim() || null,
            client_id: p.client_id.trim(),
            scopes: p.scopes,
            provision: p.provision,
            default_role: p.default_role,
            role_claim: p.role_claim.trim() || "groups",
            role_map: parseRoleMap(p.roleMapText),
            link_by_email: p.link_by_email,
            allowed_email_domains: p.allowed_email_domains,
            allowed_emails: p.allowed_emails,
            allowed_subjects: p.allowed_subjects,
            apple_team_id: p.apple_team_id?.trim() || null,
            apple_key_id: p.apple_key_id?.trim() || null,
          };
          if (p.clearSecret) {
            update.client_secret = "";
          } else if (p.client_secret?.trim()) {
            update.client_secret = p.client_secret.trim();
          }
          if (p.clearAppleKey) {
            update.apple_private_key = "";
          } else if (p.apple_private_key?.trim()) {
            update.apple_private_key = p.apple_private_key.trim();
          }
          return update;
        }),
        current_password: currentPassword.trim() || undefined,
      };
      const saved = await putOidcConfig(body);
      const draft = toDraft(saved);
      setEnabled(draft.enabled);
      setDomains(draft.domains);
      setProviders(draft.providers);
      setPublicOrigin(saved.public_origin?.trim() || "");
      setDetectedOrigin(saved.detected_origin?.trim() || pageOrigin());
      setCurrentPassword("");
      setNotice("Sign-in settings saved.");
    } catch (err) {
      setError(
        isApiError(err) ? err.message : err instanceof Error ? err.message : "Failed to save",
      );
    } finally {
      setSaving(false);
    }
  }

  if (loading) {
    return <p className="text-sm text-ink/50">Loading sign-in providers…</p>;
  }

  const issuer = previewIssuer(publicOrigin, detectedOrigin);
  const rpCallback = `${issuer}/api/auth/oidc/callback`;

  return (
    <form className="flex flex-col gap-8" onSubmit={(e) => void onSave(e)}>
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p className="text-sm font-medium text-teal" role="status">
          {notice}
        </p>
      ) : null}

      <section className="space-y-3">
        <div className="space-y-1">
          <h2 className="text-lg font-semibold text-ink">Password sign-in</h2>
          <p className="text-sm text-ink/55">
            Require a passkey or authenticator app instead of a password alone. Users without
            either can still sign in once to enroll. Passkey and SSO sign-in are unchanged.
          </p>
        </div>
        <label className="flex cursor-pointer items-center gap-2 text-sm text-ink">
          <input
            type="checkbox"
            checked={requireSecondFactor}
            onChange={(e) => setRequireSecondFactor(e.target.checked)}
          />
          Require authenticator or passkey for password login
        </label>
        <Button
          type="button"
          variant="secondary"
          className="self-start"
          disabled={savingMfa}
          onClick={() => void onSaveMfa()}
        >
          {savingMfa ? "Saving…" : "Save password policy"}
        </Button>
      </section>

      <section className="flex flex-col gap-3">
        <div className="flex flex-col gap-1">
          <h2 className="text-lg font-semibold text-ink">SSO into Bookclerk</h2>
          <p className="text-sm text-ink/55">
            Optional Google, GitHub, Apple, Discord, or company IdP buttons on the Bookclerk
            sign-in page. Bookclerk is the client. The operator token is local break-glass and
            is never an SSO account.
          </p>
        </div>
        <label className="flex cursor-pointer items-center gap-2 text-sm text-ink">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
          Show SSO buttons on the Bookclerk sign-in page
        </label>
        <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
          Global allowed email domains
          <Input
            value={domains}
            onChange={(e) => setDomains(e.target.value)}
            placeholder="family.example, corp.example"
          />
          <span className="text-xs font-normal text-ink/50">
            Extra filter on every SSO provider below. Empty means only each provider’s own
            policy applies.
          </span>
        </label>
        <p className="text-xs text-ink/55">
          Redirect URI to paste into Google, GitHub, or your IdP:{" "}
          <code className="rounded bg-ink/5 px-1 py-0.5 text-ink">{rpCallback}</code>
        </p>
      </section>

      <section className="space-y-4">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <h3 className="text-base font-semibold text-ink">SSO providers</h3>
            <p className="mt-1 text-sm text-ink/55">
              Apps that people use to sign into Bookclerk. Google, GitHub, Apple, and Discord
              only need a client ID and secret. Custom OIDC still asks for an issuer URL.
            </p>
          </div>
          <Button
            type="button"
            variant="secondary"
            onClick={() => setProviders((prev) => [...prev, emptyProvider()])}
          >
            <Plus className="h-4 w-4" />
            Add provider
          </Button>
        </div>
        {providers.length === 0 ? (
          <p className="text-sm text-ink/50">No providers configured yet.</p>
        ) : (
          <ul className="flex flex-col gap-4">
            {providers.map((provider, index) => (
              <ProviderCard
                key={`${provider.id || "new"}-${index}`}
                provider={provider}
                onChange={(next) =>
                  setProviders((prev) => prev.map((row, i) => (i === index ? next : row)))
                }
                onRemove={() =>
                  setProviders((prev) => prev.filter((_, i) => i !== index))
                }
              />
            ))}
          </ul>
        )}
      </section>

      <section className="flex flex-col gap-3">
        <div className="flex flex-col gap-1">
          <h2 className="text-lg font-semibold text-ink">Bookclerk as identity provider</h2>
          <p className="text-sm text-ink/55">
            Players and other relying parties sign into Bookclerk. Bookclerk is the OpenID
            issuer. Register each app as a client below.
          </p>
        </div>
        <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
          Public origin
          <Input
            value={publicOrigin}
            onChange={(e) => setPublicOrigin(e.target.value)}
            placeholder={detectedOrigin || pageOrigin()}
            autoComplete="url"
          />
          <span className="text-xs font-normal text-ink/50">
            Leave empty to use a bound loopback origin (
            <code className="rounded bg-ink/5 px-1">{detectedOrigin || pageOrigin()}</code>
            ). Tray and <code className="rounded bg-ink/5 px-1">cargo dev</code> use localhost.
            Production behind TLS must pin <code className="rounded bg-ink/5 px-1">https://…</code>.
            Saving empty clears a pinned origin so loopback detection keeps working.
          </span>
        </label>
        <div className="grid gap-3">
          <CopyableValue label="Issuer" value={issuer} />
          <CopyableValue
            label="Discovery"
            value={`${issuer}/.well-known/openid-configuration`}
          />
          <CopyableValue label="Authorization" value={`${issuer}/oidc/authorize`} />
          <CopyableValue label="Token" value={`${issuer}/oidc/token`} />
          <CopyableValue label="Userinfo" value={`${issuer}/oidc/userinfo`} />
        </div>
      </section>

      <section className="space-y-4">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <h3 className="text-base font-semibold text-ink">OIDC clients</h3>
            <p className="mt-1 text-sm text-ink/55">
              Apps that trust Bookclerk as their identity provider. Installed player plugins
              contribute a client (disabled until you enable it). Custom clients can still be
              added. Public PKCE clients omit a secret; confidential clients receive a generated
              secret once.
            </p>
          </div>
          <Button
            type="button"
            variant="secondary"
            onClick={() => setClients((prev) => [...prev, emptyClient()])}
          >
            <Plus className="h-4 w-4" />
            Add client
          </Button>
        </div>
        {clients.length === 0 ? (
          <p className="text-sm text-ink/50">No OIDC clients registered yet.</p>
        ) : (
          <ul className="flex flex-col gap-4">
            {clients.map((client, index) => (
              <ClientCard
                key={`${client.originalId || "new"}-${index}`}
                client={client}
                currentPassword={currentPassword.trim() || undefined}
                onChange={(next) =>
                  setClients((prev) => prev.map((row, i) => (i === index ? next : row)))
                }
                onSaved={(next) =>
                  setClients((prev) => prev.map((row, i) => (i === index ? next : row)))
                }
                onRemoved={() =>
                  setClients((prev) => prev.filter((_, i) => i !== index))
                }
                onError={setError}
                onNotice={setNotice}
              />
            ))}
          </ul>
        )}
      </section>

      <label className="space-y-1 text-sm font-medium text-ink">
        Current password
        <Input
          type="password"
          autoComplete="current-password"
          value={currentPassword}
          onChange={(e) => setCurrentPassword(e.target.value)}
          placeholder="Required if this Owner session is older than 15 minutes"
        />
        <span className="block text-xs font-normal text-ink/50">
          Operator and elevated Owner sessions skip this. A freshly signed-in Owner session is
          enough.
        </span>
      </label>

      <div>
        <Button type="submit" disabled={saving}>
          {saving ? "Saving…" : "Save sign-in settings"}
        </Button>
      </div>
    </form>
  );
}
