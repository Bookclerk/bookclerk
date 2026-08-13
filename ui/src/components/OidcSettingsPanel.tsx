import { useEffect, useState, type FormEvent } from "react";
import { Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  fetchOidcConfig,
  isApiError,
  putOidcConfig,
  type OidcBrokerConfigView,
  type OidcProvisionMode,
  type OidcProviderConfigUpdate,
  type OidcSecretSource,
} from "@/lib/api";

const selectClassName =
  "h-9 w-full rounded-md border border-ink/15 bg-white/80 px-2 text-sm text-ink outline-none focus:border-teal";

const PRESETS = [
  ["", "Custom OIDC"],
  ["google", "Google"],
  ["github", "GitHub"],
  ["apple", "Apple"],
  ["discord", "Discord"],
] as const;

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

const PROVISIONS: [OidcProvisionMode, string][] = [
  ["mapped_role", "Mapped role (enterprise groups)"],
  ["any", "Any authenticated account"],
  ["allowlist", "Allowlist (email / domain / subject)"],
  ["invite_only", "Invite only (no JIT)"],
];

type DraftProvider = OidcProviderConfigUpdate & {
  has_client_secret: boolean;
  has_apple_private_key: boolean;
  secret_source: OidcSecretSource;
  clearSecret: boolean;
  clearAppleKey: boolean;
  roleMapText: string;
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
    })),
  };
}

function emptyProvider(): DraftProvider {
  return {
    id: "",
    name: "",
    preset: "",
    issuer: "",
    client_id: "",
    client_secret: "",
    scopes: ["openid", "profile", "email"],
    provision: "mapped_role",
    default_role: "member",
    role_claim: "groups",
    role_map: {},
    link_by_email: false,
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
  };
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

/**
 * Owner/operator Settings tab for the optional OIDC/OAuth identity broker.
 */
export function OidcSettingsPanel() {
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [callbackUrl, setCallbackUrl] = useState<string | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [domains, setDomains] = useState("");
  const [providers, setProviders] = useState<DraftProvider[]>([]);
  const [currentPassword, setCurrentPassword] = useState("");

  async function reload() {
    const view = await fetchOidcConfig();
    const draft = toDraft(view);
    setEnabled(draft.enabled);
    setDomains(draft.domains);
    setProviders(draft.providers);
    setCallbackUrl(view.callback_url?.trim() || null);
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

  function updateProvider(index: number, patch: Partial<DraftProvider>) {
    setProviders((prev) => prev.map((p, i) => (i === index ? { ...p, ...patch } : p)));
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
        providers: providers.map((p) => {
          const update: OidcProviderConfigUpdate = {
            id: p.id.trim(),
            name: p.name.trim(),
            preset: canonicalSocialPreset(p.preset),
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
      setCallbackUrl(saved.callback_url?.trim() || null);
      setCurrentPassword("");
      setNotice("Sign-in providers saved.");
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
          <h2 className="text-lg font-semibold text-ink">Identity broker</h2>
          <p className="text-sm text-ink/55">
            Optional upstream OIDC or OAuth providers for User sign-in. Bookclerk stays the
            Audiobookshelf issuer; the Operator token is never an OAuth subject.
          </p>
        </div>
        <label className="flex cursor-pointer items-center gap-2 text-sm text-ink">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
          Enable SSO login buttons
        </label>
        <label className="block space-y-1.5 text-sm font-medium text-ink">
          Global allowed email domains
          <Input
            value={domains}
            onChange={(e) => setDomains(e.target.value)}
            placeholder="family.example, corp.example"
          />
          <span className="block text-xs font-normal text-ink/50">
            Applied after each provider’s own policy. Empty means no extra global filter.
          </span>
        </label>
        {callbackUrl ? (
          <p className="text-xs text-ink/55">
            Redirect URI for every provider:{" "}
            <code className="rounded bg-ink/5 px-1 py-0.5 text-ink">{callbackUrl}</code>
          </p>
        ) : (
          <p className="text-xs text-ink/55">
            Set <code className="rounded bg-ink/5 px-1">integrations.public_origin</code> so
            providers can use{" "}
            <code className="rounded bg-ink/5 px-1">
              {"{public_origin}/api/auth/oidc/callback"}
            </code>
            .
          </p>
        )}
      </section>

      <section className="space-y-4">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-base font-semibold text-ink">Providers</h3>
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
              <li
                key={`${provider.id || "new"}-${index}`}
                className="space-y-4 rounded-md border border-ink/10 bg-white/40 p-4"
              >
                <div className="flex items-start justify-between gap-2">
                  <p className="text-sm font-semibold text-ink">
                    {provider.name.trim() || provider.id.trim() || "New provider"}
                  </p>
                  <Button
                    type="button"
                    variant="ghost"
                    onClick={() =>
                      setProviders((prev) => prev.filter((_, i) => i !== index))
                    }
                    aria-label="Remove provider"
                  >
                    <Trash2 className="h-4 w-4" />
                  </Button>
                </div>
                <div className="grid gap-3 sm:grid-cols-2">
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Id
                    <Input
                      value={provider.id}
                      onChange={(e) => updateProvider(index, { id: e.target.value })}
                      placeholder="github"
                      required
                    />
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Display name
                    <Input
                      value={provider.name}
                      onChange={(e) => updateProvider(index, { name: e.target.value })}
                      placeholder="GitHub"
                    />
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Preset
                    <select
                      className={selectClassName}
                      value={provider.preset ?? ""}
                      onChange={(e) => updateProvider(index, { preset: e.target.value })}
                    >
                      {PRESETS.map(([value, label]) => (
                        <option key={value || "custom"} value={value}>
                          {label}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Client ID
                    <Input
                      value={provider.client_id}
                      onChange={(e) => updateProvider(index, { client_id: e.target.value })}
                      required
                    />
                  </label>
                  {!provider.preset ? (
                    <label className="space-y-1 text-sm font-medium text-ink sm:col-span-2">
                      Issuer URL
                      <Input
                        value={provider.issuer ?? ""}
                        onChange={(e) => updateProvider(index, { issuer: e.target.value })}
                        placeholder="https://idp.example.com/realms/corp"
                      />
                    </label>
                  ) : null}
                  <label className="space-y-1 text-sm font-medium text-ink sm:col-span-2">
                    Client secret
                    <Input
                      type="password"
                      autoComplete="new-password"
                      value={provider.client_secret ?? ""}
                      onChange={(e) =>
                        updateProvider(index, {
                          client_secret: e.target.value,
                          clearSecret: false,
                        })
                      }
                      placeholder={provider.has_client_secret ? "unchanged" : "optional"}
                      disabled={provider.clearSecret}
                    />
                    <span className="block text-xs font-normal text-ink/50">
                      {secretHint(provider.secret_source, provider.has_client_secret)}
                    </span>
                    {provider.has_client_secret ? (
                      <label className="mt-1 flex items-center gap-2 text-xs font-normal text-ink/70">
                        <input
                          type="checkbox"
                          checked={provider.clearSecret}
                          onChange={(e) =>
                            updateProvider(index, {
                              clearSecret: e.target.checked,
                              client_secret: e.target.checked ? "" : provider.client_secret,
                            })
                          }
                        />
                        Clear stored secret
                      </label>
                    ) : null}
                  </label>
                  {canonicalSocialPreset(provider.preset) === "apple" ? (
                    <>
                      <label className="space-y-1 text-sm font-medium text-ink">
                        Apple Team ID
                        <Input
                          value={provider.apple_team_id ?? ""}
                          onChange={(e) =>
                            updateProvider(index, { apple_team_id: e.target.value })
                          }
                          placeholder="AB12CDEF34"
                        />
                      </label>
                      <label className="space-y-1 text-sm font-medium text-ink">
                        Apple Key ID
                        <Input
                          value={provider.apple_key_id ?? ""}
                          onChange={(e) =>
                            updateProvider(index, { apple_key_id: e.target.value })
                          }
                          placeholder="WXYZ123456"
                        />
                      </label>
                      <label className="space-y-1 text-sm font-medium text-ink sm:col-span-2">
                        Apple private key (.p8)
                        <Input
                          type="password"
                          autoComplete="new-password"
                          value={provider.apple_private_key ?? ""}
                          onChange={(e) =>
                            updateProvider(index, {
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
                          <label className="mt-1 flex items-center gap-2 text-xs font-normal text-ink/70">
                            <input
                              type="checkbox"
                              checked={provider.clearAppleKey}
                              onChange={(e) =>
                                updateProvider(index, {
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
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Provisioning
                    <select
                      className={selectClassName}
                      value={provider.provision}
                      onChange={(e) =>
                        updateProvider(index, {
                          provision: e.target.value as OidcProvisionMode,
                        })
                      }
                    >
                      {PROVISIONS.map(([value, label]) => (
                        <option key={value} value={value}>
                          {label}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Default role
                    <select
                      className={selectClassName}
                      value={provider.default_role}
                      onChange={(e) => updateProvider(index, { default_role: e.target.value })}
                    >
                      <option value="member">Member</option>
                      <option value="administrator">Administrator</option>
                      <option value="owner">Owner</option>
                    </select>
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink sm:col-span-2">
                    Scopes
                    <Input
                      value={csv(provider.scopes)}
                      onChange={(e) =>
                        updateProvider(index, { scopes: splitCsv(e.target.value) })
                      }
                    />
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Role claim
                    <Input
                      value={provider.role_claim}
                      onChange={(e) => updateProvider(index, { role_claim: e.target.value })}
                      placeholder="groups"
                    />
                  </label>
                  <label className="flex items-end gap-2 pb-2 text-sm font-medium text-ink">
                    <input
                      type="checkbox"
                      checked={provider.link_by_email}
                      onChange={(e) =>
                        updateProvider(index, { link_by_email: e.target.checked })
                      }
                    />
                      Link by verified email
                    </label>
                  <label className="space-y-1 text-sm font-medium text-ink sm:col-span-2">
                    Role map
                    <textarea
                      className="min-h-[4.5rem] w-full rounded-md border border-ink/15 bg-white/80 px-3 py-2 text-sm text-ink shadow-sm placeholder:text-ink/40 focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
                      value={provider.roleMapText}
                      onChange={(e) => updateProvider(index, { roleMapText: e.target.value })}
                      placeholder={"bookclerk-owners = owner\nbookclerk-admins = administrator"}
                    />
                    <span className="block text-xs font-normal text-ink/50">
                      One <code>claim = owner|administrator|member</code> per line. Never{" "}
                      <code>operator</code>.
                    </span>
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Allowed email domains
                    <Input
                      value={csv(provider.allowed_email_domains)}
                      onChange={(e) =>
                        updateProvider(index, {
                          allowed_email_domains: splitCsv(e.target.value),
                        })
                      }
                    />
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink">
                    Allowed emails
                    <Input
                      value={csv(provider.allowed_emails)}
                      onChange={(e) =>
                        updateProvider(index, { allowed_emails: splitCsv(e.target.value) })
                      }
                    />
                  </label>
                  <label className="space-y-1 text-sm font-medium text-ink sm:col-span-2">
                    Allowed subjects
                    <Input
                      value={csv(provider.allowed_subjects)}
                      onChange={(e) =>
                        updateProvider(index, {
                          allowed_subjects: splitCsv(e.target.value),
                        })
                      }
                    />
                  </label>
                </div>
              </li>
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
          {saving ? "Saving…" : "Save sign-in providers"}
        </Button>
      </div>
    </form>
  );
}
