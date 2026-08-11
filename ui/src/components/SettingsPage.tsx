import { useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";
import { ChevronDown, ChevronRight, RefreshCw } from "lucide-react";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import { ErrorStatePage } from "@/components/ErrorStatePage";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  approvePluginConsent,
  fetchPluginConsent,
  fetchSettings,
  isApiError,
  listUsers,
  patchSettings,
  startImpersonate,
  type AuthSession,
  type ListedUser,
  type PluginConsentResponse,
  type PluginSettingOption,
  type PluginSettingsGroup,
  signOut,
  type AuthRole,
  type SettingsResponse,
} from "@/lib/api";
import { googleFaviconUrl, storeFaviconUrl } from "@/lib/catalogTitle";
import { cn, pageWidthClass } from "@/lib/utils";

const KIND_LABELS: Record<string, string> = {
  source: "Sources",
  destination: "Destinations",
  database: "Database",
  integration: "Integrations",
  other: "Other",
};

function kindLabel(kind: string): string {
  const key = kind.trim().toLowerCase();
  if (KIND_LABELS[key]) return KIND_LABELS[key];
  if (!key) return "Other";
  return key.charAt(0).toUpperCase() + key.slice(1);
}

const DEFAULT_DAEMON_PORT = "8787";

type ListenExposure = "localhost" | "all" | "custom";
type ListenRow = { host: string; port: string };

/** Split one `daemon.listen` entry (`127.0.0.1:8787` / `[::1]:8787`) into host + port. */
function splitDaemonListen(listen: string): ListenRow {
  const trimmed = listen.trim();
  if (!trimmed) {
    return { host: "127.0.0.1", port: DEFAULT_DAEMON_PORT };
  }
  const bracketed = trimmed.match(/^\[([^\]]+)\]:(\d+)$/);
  if (bracketed) {
    return { host: bracketed[1], port: bracketed[2] };
  }
  const idx = trimmed.lastIndexOf(":");
  if (idx > 0) {
    const host = trimmed.slice(0, idx);
    const port = trimmed.slice(idx + 1);
    if (/^\d+$/.test(port) && !host.includes(":")) {
      return { host, port };
    }
  }
  return { host: trimmed, port: DEFAULT_DAEMON_PORT };
}

/** Join host + port into a Rust `SocketAddr` string (IPv6 bracketed). */
function joinDaemonListen(host: string, port: string): string {
  const h = host.trim() || "127.0.0.1";
  const p = port.trim() || DEFAULT_DAEMON_PORT;
  const normalized =
    h.toLowerCase() === "localhost" || h.toLowerCase() === "localhost."
      ? "127.0.0.1"
      : h.replace(/^\[|\]$/g, "");
  if (normalized.includes(":")) {
    return `[${normalized}]:${p}`;
  }
  return `${normalized}:${p}`;
}

function parseListenList(raw: string): ListenRow[] {
  const parts = raw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  if (parts.length === 0) {
    return [
      { host: "127.0.0.1", port: DEFAULT_DAEMON_PORT },
      { host: "::1", port: DEFAULT_DAEMON_PORT },
    ];
  }
  return parts.map(splitDaemonListen);
}

function detectListenExposure(rows: ListenRow[]): ListenExposure {
  if (rows.length === 2) {
    const ports = new Set(rows.map((r) => r.port.trim()));
    if (ports.size === 1) {
      const hosts = new Set(rows.map((r) => r.host.trim()));
      if (hosts.has("127.0.0.1") && hosts.has("::1")) return "localhost";
      if (hosts.has("0.0.0.0") && hosts.has("::")) return "all";
    }
  }
  return "custom";
}

function listenListFromExposure(exposure: Exclude<ListenExposure, "custom">, port: string): string {
  const p = port.trim() || DEFAULT_DAEMON_PORT;
  if (exposure === "localhost") {
    return `${joinDaemonListen("127.0.0.1", p)},${joinDaemonListen("::1", p)}`;
  }
  return `${joinDaemonListen("0.0.0.0", p)},${joinDaemonListen("::", p)}`;
}

function joinListenRows(rows: ListenRow[]): string {
  return rows
    .filter((r) => r.host.trim())
    .map((r) => joinDaemonListen(r.host, r.port))
    .join(",");
}

function daemonPortError(port: string): string | null {
  const trimmed = port.trim();
  if (!trimmed) return "Port is required";
  if (!/^\d+$/.test(trimmed)) return "Port must be a number";
  const n = Number(trimmed);
  if (n < 1 || n > 65535) return "Port must be between 1 and 65535";
  return null;
}

function pluginRowKey(plugin: PluginSettingsGroup): string {
  return `${plugin.kind}:${plugin.id}`;
}

/** Prefer the standard `*.enabled` knob; fall back to an "Enabled" label. */
function findEnabledOption(plugin: PluginSettingsGroup): PluginSettingOption | null {
  const byKey = plugin.settings.find((option) => option.key.endsWith(".enabled"));
  if (byKey) return byKey;
  return plugin.settings.find((option) => option.label.trim().toLowerCase() === "enabled") ?? null;
}

/** Prefer API `logo`, then known store/domain favicons; advance on load failure. */
function pluginLogoCandidates(plugin: PluginSettingsGroup): string[] {
  const out: string[] = [];
  const push = (url: string | undefined) => {
    const t = url?.trim();
    if (t && !out.includes(t)) out.push(t);
  };
  push(plugin.logo);
  if (plugin.kind === "source" || plugin.kind === "integration") {
    push(storeFaviconUrl(plugin.id));
  }
  const domain = PLUGIN_FAVICON_FALLBACK_DOMAINS[pluginRowKey(plugin)];
  if (domain) push(googleFaviconUrl(domain));
  return out;
}

/** Domains when API logo is missing or the primary `<img>` fails to load. */
const PLUGIN_FAVICON_FALLBACK_DOMAINS: Record<string, string> = {
  "source:audible": "audible.com",
  "source:chirp": "chirpbooks.com",
  "source:libro": "libro.fm",
  "source:graphicaudio": "graphicaudio.com",
  "integration:audiobookshelf": "audiobookshelf.org",
  "database:sqlite": "sqlite.org",
  "database:d1": "cloudflare.com",
  "database:postgres": "postgresql.org",
  "output:s3": "aws.amazon.com",
};

function PluginLogo({ plugin }: { plugin: PluginSettingsGroup }) {
  const candidates = pluginLogoCandidates(plugin);
  const [index, setIndex] = useState(0);
  useEffect(() => {
    setIndex(0);
  }, [plugin.kind, plugin.id, plugin.logo]);
  const src = candidates[index];
  if (!src) {
    return (
      <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded bg-fold text-[10px] font-semibold uppercase text-ink/70">
        {plugin.id.slice(0, 2)}
      </div>
    );
  }
  return (
    <img
      key={src}
      src={src}
      alt=""
      className="h-7 w-7 shrink-0 rounded bg-white object-contain p-0.5"
      onError={() => setIndex((i) => i + 1)}
    />
  );
}

function ToggleRow({
  id,
  label,
  description,
  checked,
  onChange,
  disabled,
}: {
  id: string;
  label: string;
  description?: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <div className="flex items-start justify-between gap-4 py-3">
      <div className="min-w-0 space-y-0.5">
        <label htmlFor={id} className="text-sm font-medium text-ink">
          {label}
        </label>
        {description ? <p className="text-xs text-ink/50">{description}</p> : null}
      </div>
      <input
        id={id}
        type="checkbox"
        className="mt-1 h-4 w-4 shrink-0 accent-teal"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
      />
    </div>
  );
}

function FieldBlock({
  label,
  htmlFor,
  hint,
  error,
  children,
}: {
  label: string;
  htmlFor?: string;
  hint?: string;
  error?: string;
  children: ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <label htmlFor={htmlFor} className="block text-sm font-medium text-ink">
        {label}
      </label>
      {children}
      {hint && !error ? <p className="text-xs text-ink/50">{hint}</p> : null}
      {error ? (
        <p className="text-xs text-brick" role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}

const selectClassName =
  "w-full rounded-md border border-ink/15 bg-white/80 px-3 py-2 text-sm text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

export function SettingsPage({
  onLogout,
  onSessionExpired,
  onSessionChange,
  nav,
  role,
  session,
}: {
  onLogout: () => void;
  onSessionExpired: () => void;
  onSessionChange?: () => void | Promise<void>;
  nav: AppNavProps;
  role?: AuthRole;
  session?: AuthSession | null;
}) {
  const [settings, setSettings] = useState<SettingsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [operatorLoadError, setOperatorLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [listenExposure, setListenExposure] = useState<ListenExposure>("localhost");
  const [daemonPort, setDaemonPort] = useState(DEFAULT_DAEMON_PORT);
  const [advancedRows, setAdvancedRows] = useState<ListenRow[]>([
    { host: "127.0.0.1", port: DEFAULT_DAEMON_PORT },
    { host: "::1", port: DEFAULT_DAEMON_PORT },
  ]);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [daemonAuthEnabled, setDaemonAuthEnabled] = useState(true);
  const [autoAcquire, setAutoAcquire] = useState(false);
  const [pluginValues, setPluginValues] = useState<Record<string, string>>({});
  const [pluginErrors, setPluginErrors] = useState<Record<string, string>>({});
  /** Plugins start collapsed; keys are `${kind}:${id}`. */
  const [expandedPlugins, setExpandedPlugins] = useState<Set<string>>(() => new Set());
  const [consentPrompt, setConsentPrompt] = useState<PluginConsentResponse | null>(null);
  const [consentBusy, setConsentBusy] = useState(false);
  const [operatorBaseline, setOperatorBaseline] = useState<{
    daemonListen: string;
    daemonAuthEnabled: boolean;
    autoAcquire: boolean;
    pluginValues: Record<string, string>;
  } | null>(null);
  const [users, setUsers] = useState<ListedUser[]>([]);
  const [impersonateBusy, setImpersonateBusy] = useState(false);

  const daemonListen =
    listenExposure === "custom"
      ? joinListenRows(advancedRows)
      : listenListFromExposure(listenExposure, daemonPort);
  const daemonListenError =
    listenExposure === "custom"
      ? advancedRows.some((r) => r.host.trim() && daemonPortError(r.port))
        ? "Each bind needs a valid port (1–65535)."
        : advancedRows.every((r) => !r.host.trim())
          ? "Add at least one listen address."
          : null
      : daemonPortError(daemonPort);

  function buildPluginValues(nextSettings: SettingsResponse): Record<string, string> {
    const out: Record<string, string> = {};
    for (const plugin of nextSettings.plugins) {
      for (const option of plugin.settings) {
        out[option.key] = option.value;
      }
    }
    return out;
  }

  async function withRequestTimeout<T>(promise: Promise<T>, label: string, timeoutMs = 10_000): Promise<T> {
    let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
    try {
      const timeoutPromise = new Promise<never>((_, reject) => {
        timeoutHandle = setTimeout(() => {
          reject(new Error(`${label} timed out after ${Math.floor(timeoutMs / 1000)}s`));
        }, timeoutMs);
      });
      return await Promise.race([promise, timeoutPromise]);
    } finally {
      if (timeoutHandle) clearTimeout(timeoutHandle);
    }
  }

  function parseBooleanLike(value: string): boolean {
    const normalized = value.trim().toLowerCase();
    return normalized === "1" || normalized === "true" || normalized === "yes" || normalized === "on";
  }

  function pluginValidationForOption(value: string, option: PluginSettingOption): string | null {
    if (option.value_type === "boolean") {
      return null;
    }
    if (option.choices?.length) {
      return option.choices.some((choice) => choice.value === value)
        ? null
        : "Choose a valid option";
    }
    if (option.value_type === "number") {
      if (!value.trim()) {
        return null;
      }
      if (Number.isNaN(Number(value))) {
        return "Enter a valid number";
      }
      return null;
    }
    return null;
  }

  function setPluginValue(option: PluginSettingOption, nextValue: string) {
    const settingKey = option.key;
    setPluginValues((current) => ({
      ...current,
      [settingKey]: nextValue,
    }));
    const validation = pluginValidationForOption(nextValue, option);
    setPluginErrors((current) => {
      if (!validation) {
        const { [settingKey]: _removed, ...rest } = current;
        return rest;
      }
      return {
        ...current,
        [settingKey]: validation,
      };
    });
  }

  const operatorHasValidationErrors =
    Object.keys(pluginErrors).length > 0 || daemonListenError !== null;

  const pluginsByKind = useMemo(() => {
    const buckets = new Map<string, PluginSettingsGroup[]>();
    if (!settings) {
      return buckets;
    }
    for (const plugin of settings.plugins) {
      const kind = plugin.kind || "other";
      const current = buckets.get(kind) ?? [];
      current.push(plugin);
      buckets.set(kind, current);
    }
    return buckets;
  }, [settings]);

  const operatorDirty = useMemo(() => {
    if (!operatorBaseline) {
      return false;
    }
    if (daemonListen !== operatorBaseline.daemonListen) {
      return true;
    }
    if (daemonAuthEnabled !== operatorBaseline.daemonAuthEnabled) {
      return true;
    }
    if (autoAcquire !== operatorBaseline.autoAcquire) {
      return true;
    }
    const currentKeys = Object.keys(pluginValues);
    const baselineKeys = Object.keys(operatorBaseline.pluginValues);
    if (currentKeys.length !== baselineKeys.length) {
      return true;
    }
    return currentKeys.some((key) => pluginValues[key] !== operatorBaseline.pluginValues[key]);
  }, [autoAcquire, daemonAuthEnabled, daemonListen, operatorBaseline, pluginValues]);

  async function refresh() {
    setError(null);
    setOperatorLoadError(null);
    setLoading(true);
    try {
      if (role !== "operator") {
        setSettings(null);
        setPluginValues({});
        setPluginErrors({});
        setOperatorBaseline(null);
        return;
      }

      const nextSettings = await withRequestTimeout(
        fetchSettings(),
        "Operator settings request",
      );
      const nextPluginValues = buildPluginValues(nextSettings);
      setSettings(nextSettings);
      const listen = nextSettings.settings["daemon.listen"] ?? "";
      const rows = parseListenList(listen);
      const exposure = detectListenExposure(rows);
      setListenExposure(exposure);
      setDaemonPort(rows[0]?.port ?? DEFAULT_DAEMON_PORT);
      setAdvancedRows(rows);
      setAdvancedOpen(exposure === "custom");
      setDaemonAuthEnabled(nextSettings.settings["daemon.auth.enabled"] === "true");
      setAutoAcquire(nextSettings.settings["library.auto_acquire"] === "true");
      setPluginValues(nextPluginValues);
      setPluginErrors({});
      setOperatorBaseline({
        daemonListen:
          exposure === "custom"
            ? joinListenRows(rows)
            : listenListFromExposure(exposure, rows[0]?.port ?? DEFAULT_DAEMON_PORT),
        daemonAuthEnabled: nextSettings.settings["daemon.auth.enabled"] === "true",
        autoAcquire: nextSettings.settings["library.auto_acquire"] === "true",
        pluginValues: nextPluginValues,
      });
      try {
        setUsers(await listUsers());
      } catch {
        setUsers([]);
      }
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setSettings(null);
      setOperatorLoadError(
        err instanceof Error ? err.message : "Failed to load operator settings",
      );
      setError(
        err instanceof Error
          ? err.message
          : "Failed to load settings. If this persists, verify bookclerkd is running.",
      );
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  async function saveOperatorSettings(): Promise<SettingsResponse> {
    const pluginUpdates = Object.entries(pluginValues).map(([key, value]) => ({ key, value }));
    const nextListen = daemonListen;
    return withRequestTimeout(
      patchSettings({
        settings: [
          { key: "daemon.listen", value: nextListen },
          { key: "daemon.auth.enabled", value: String(daemonAuthEnabled) },
          { key: "library.auto_acquire", value: String(autoAcquire) },
          ...pluginUpdates,
        ],
      }),
      "Save operator settings",
    );
  }

  function applySavedSettings(next: SettingsResponse, nextListenFallback: string) {
    const nextPluginValues = buildPluginValues(next);
    setSettings(next);
    const savedListen = next.settings["daemon.listen"] ?? nextListenFallback;
    const rows = parseListenList(savedListen);
    const exposure = detectListenExposure(rows);
    setListenExposure(exposure);
    setDaemonPort(rows[0]?.port ?? DEFAULT_DAEMON_PORT);
    setAdvancedRows(rows);
    setAdvancedOpen(exposure === "custom");
    setPluginValues(nextPluginValues);
    setPluginErrors({});
    setOperatorBaseline({
      daemonListen:
        exposure === "custom"
          ? joinListenRows(rows)
          : listenListFromExposure(exposure, rows[0]?.port ?? DEFAULT_DAEMON_PORT),
      daemonAuthEnabled: next.settings["daemon.auth.enabled"] === "true",
      autoAcquire: next.settings["library.auto_acquire"] === "true",
      pluginValues: nextPluginValues,
    });
  }

  async function promptConsentForPlugin(pluginId: string, fallbackSummary?: string[]) {
    try {
      const consent = await fetchPluginConsent(pluginId);
      setConsentPrompt(consent);
    } catch {
      setConsentPrompt({
        plugin_id: pluginId,
        request: {
          pluginId,
          kind: "",
          networkMode: "",
          domains: [],
          bindings: [],
          compatibilityFlags: [],
          approvedAt: "",
        },
        covered: false,
        summary: fallbackSummary?.length
          ? fallbackSummary
          : [
              `Plugin: ${pluginId}`,
              "Approve network mode and host bindings before enabling.",
              "Workerd plugins may list outbound domains (enforced in the isolate).",
              "Native outbound has no hostname allowlist — coarse jail internet only.",
            ],
      });
    }
  }

  async function onOperatorSave(e: FormEvent) {
    e.preventDefault();
    if (!operatorDirty || operatorHasValidationErrors) {
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const next = await saveOperatorSettings();
      applySavedSettings(next, daemonListen);
      setConsentPrompt(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      if (isApiError(err) && err.code === "consent_required" && err.pluginId) {
        await promptConsentForPlugin(err.pluginId, err.summary);
        setError(null);
        return;
      }
      setError(err instanceof Error ? err.message : "Failed to save operator settings");
    } finally {
      setSaving(false);
    }
  }

  async function onConsentApprove() {
    if (!consentPrompt) return;
    setConsentBusy(true);
    setError(null);
    try {
      await approvePluginConsent(consentPrompt.plugin_id);
      const next = await saveOperatorSettings();
      applySavedSettings(next, daemonListen);
      setConsentPrompt(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      if (isApiError(err) && err.code === "consent_required" && err.pluginId) {
        await promptConsentForPlugin(err.pluginId, err.summary);
        return;
      }
      setError(err instanceof Error ? err.message : "Failed to approve plugin permissions");
    } finally {
      setConsentBusy(false);
    }
  }

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  function renderPluginOption(plugin: PluginSettingsGroup, option: PluginSettingOption) {
    const fieldId = `${plugin.kind}-${plugin.id}-${option.key}`;
    const value = pluginValues[option.key] ?? option.value;
    const error = pluginErrors[option.key];

    if (option.value_type === "boolean") {
      return (
        <ToggleRow
          key={fieldId}
          id={fieldId}
          label={option.label}
          checked={parseBooleanLike(value)}
          onChange={(checked) => setPluginValue(option, String(checked))}
        />
      );
    }

    if (option.choices?.length) {
      return (
        <FieldBlock key={fieldId} label={option.label} htmlFor={fieldId} error={error}>
          <select
            id={fieldId}
            value={value}
            onChange={(e) => setPluginValue(option, e.target.value)}
            className={selectClassName}
          >
            {option.choices.map((choice) => (
              <option key={`${option.key}-${choice.value}`} value={choice.value}>
                {choice.label}
              </option>
            ))}
          </select>
        </FieldBlock>
      );
    }

    return (
      <FieldBlock key={fieldId} label={option.label} htmlFor={fieldId} error={error}>
        <Input
          id={fieldId}
          type={option.value_type === "number" ? "number" : "text"}
          value={value}
          onChange={(e) => setPluginValue(option, e.target.value)}
        />
      </FieldBlock>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className={pageWidthClass}>
          <AppTopBar
            nav={nav}
            onSignOut={onSignOut}
            actions={
              role === "operator" ? (
                <Button
                  variant="secondary"
                  onClick={() => void refresh()}
                  disabled={loading || saving}
                >
                  <RefreshCw className="h-4 w-4" />
                  Refresh
                </Button>
              ) : undefined
            }
          />
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto">
      <main className={cn("flex w-full flex-col gap-8 px-4 py-6 sm:px-5", pageWidthClass)}>
        <div className="space-y-1">
          <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">Settings</h1>
          <p className="text-sm text-ink/60">
            {role === "operator"
              ? "Daemon, library, and plugin knobs for this Bookclerk host."
              : "User preferences are under Preferences in the header menu."}
          </p>
        </div>

        {error && role !== "operator" ? (
          <ErrorStatePage
            title="Settings request failed"
            message={error}
            onRetry={() => void refresh()}
          />
        ) : null}

        {role === "operator" && users.length > 0 ? (
          <section className="space-y-3">
            <div className="space-y-1">
              <h2 className="text-lg font-semibold text-ink">Impersonate</h2>
              <p className="text-sm text-ink/55">
                View the library as another user. A banner appears until you stop.
              </p>
            </div>
            <ul className="divide-y divide-ink/10 bg-white/35">
              {users.map((u) => (
                <li
                  key={u.id}
                  className="flex items-center justify-between gap-3 px-3 py-2 text-sm"
                >
                  <div>
                    <div className="font-medium text-ink">
                      {u.display_name?.trim() || `User #${u.id}`}
                    </div>
                    <div className="text-xs text-ink/50">
                      {u.role} · {u.status}
                    </div>
                  </div>
                  <Button
                    type="button"
                    variant="secondary"
                    disabled={
                      impersonateBusy ||
                      session?.impersonating?.user_id === u.id
                    }
                    onClick={() => {
                      void (async () => {
                        setImpersonateBusy(true);
                        try {
                          await startImpersonate(u.id);
                          await onSessionChange?.();
                        } catch (err) {
                          setError(
                            err instanceof Error
                              ? err.message
                              : "Impersonate failed",
                          );
                        } finally {
                          setImpersonateBusy(false);
                        }
                      })();
                    }}
                  >
                    {session?.impersonating?.user_id === u.id
                      ? "Active"
                      : "Impersonate"}
                  </Button>
                </li>
              ))}
            </ul>
          </section>
        ) : null}

        {role === "operator" ? (
          loading ? (
            <p className="text-sm text-ink/50">Loading operator settings…</p>
          ) : settings ? (
            <form
              id="operator-settings-form"
              className="flex flex-col gap-10"
              onSubmit={(e) => void onOperatorSave(e)}
            >
              {error ? (
                <p className="text-sm font-medium text-brick" role="alert">
                  {error}
                </p>
              ) : null}

              <section className="space-y-3">
                <div className="space-y-1">
                  <h2 className="text-lg font-semibold text-ink">Daemon</h2>
                  <p className="text-sm text-ink/55">
                    How bookclerkd listens and whether operator auth is required.
                  </p>
                </div>
                <div className="divide-y divide-ink/10 bg-white/35 px-3">
                  <div className="space-y-4 py-3">
                    <div className="space-y-2">
                      <p className="text-sm font-medium text-ink">Listen on</p>
                      <div className="flex flex-wrap gap-2">
                        {(
                          [
                            ["localhost", "Localhost"],
                            ["all", "All interfaces"],
                          ] as const
                        ).map(([id, label]) => (
                          <Button
                            key={id}
                            type="button"
                            variant={listenExposure === id ? "secondary" : "ghost"}
                            className="px-3 py-1.5 text-sm"
                            onClick={() => {
                              setListenExposure(id);
                              setAdvancedOpen(false);
                              const p = daemonPort.trim() || DEFAULT_DAEMON_PORT;
                              setAdvancedRows(
                                id === "localhost"
                                  ? [
                                      { host: "127.0.0.1", port: p },
                                      { host: "::1", port: p },
                                    ]
                                  : [
                                      { host: "0.0.0.0", port: p },
                                      { host: "::", port: p },
                                    ],
                              );
                            }}
                          >
                            {label}
                          </Button>
                        ))}
                        {listenExposure === "custom" ? (
                          <Badge className="bg-ink/5 text-ink/60 normal-case tracking-normal">
                            Custom
                          </Badge>
                        ) : null}
                      </div>
                      <p className="text-xs text-ink/50">
                        Localhost binds both 127.0.0.1 and ::1. All interfaces binds 0.0.0.0 and ::.
                        The tray opens {"http://localhost:<port>"}.
                      </p>
                    </div>

                    {listenExposure !== "custom" ? (
                      <FieldBlock
                        label="Port"
                        htmlFor="daemon-port"
                        error={daemonListenError ?? undefined}
                      >
                        <Input
                          id="daemon-port"
                          className="max-w-[8rem]"
                          inputMode="numeric"
                          value={daemonPort}
                          onChange={(e) => {
                            const p = e.target.value;
                            setDaemonPort(p);
                            setAdvancedRows((rows) =>
                              rows.map((r) => ({ ...r, port: p || DEFAULT_DAEMON_PORT })),
                            );
                          }}
                          placeholder={DEFAULT_DAEMON_PORT}
                          autoComplete="off"
                          spellCheck={false}
                        />
                      </FieldBlock>
                    ) : null}

                    <details
                      className="rounded-md border border-ink/10 bg-paper/40"
                      open={advancedOpen}
                      onToggle={(e) => setAdvancedOpen(e.currentTarget.open)}
                    >
                      <summary className="cursor-pointer px-3 py-2 text-sm font-medium text-ink">
                        Advanced bind addresses
                      </summary>
                      <div className="space-y-3 border-t border-ink/10 px-3 py-3">
                        <p className="text-xs text-ink/50">
                          Bind each address separately. Editing here switches to a custom listen list.
                        </p>
                        {advancedRows.map((row, index) => (
                          <div
                            key={`listen-row-${index}`}
                            className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_7rem_auto]"
                          >
                            <Input
                              aria-label={`Listen address ${index + 1}`}
                              value={row.host}
                              onChange={(e) => {
                                const host = e.target.value;
                                setListenExposure("custom");
                                setAdvancedRows((rows) =>
                                  rows.map((r, i) => (i === index ? { ...r, host } : r)),
                                );
                              }}
                              placeholder="127.0.0.1"
                              autoComplete="off"
                              spellCheck={false}
                            />
                            <Input
                              aria-label={`Listen port ${index + 1}`}
                              inputMode="numeric"
                              value={row.port}
                              onChange={(e) => {
                                const port = e.target.value;
                                setListenExposure("custom");
                                setAdvancedRows((rows) =>
                                  rows.map((r, i) => (i === index ? { ...r, port } : r)),
                                );
                              }}
                              placeholder={DEFAULT_DAEMON_PORT}
                              autoComplete="off"
                              spellCheck={false}
                            />
                            <Button
                              type="button"
                              variant="ghost"
                              className="px-2"
                              disabled={advancedRows.length <= 1}
                              onClick={() => {
                                setListenExposure("custom");
                                setAdvancedRows((rows) => rows.filter((_, i) => i !== index));
                              }}
                            >
                              Remove
                            </Button>
                          </div>
                        ))}
                        {daemonListenError && listenExposure === "custom" ? (
                          <p className="text-xs text-brick" role="alert">
                            {daemonListenError}
                          </p>
                        ) : null}
                        <Button
                          type="button"
                          variant="secondary"
                          className="text-sm"
                          onClick={() => {
                            setListenExposure("custom");
                            setAdvancedRows((rows) => [
                              ...rows,
                              {
                                host: "",
                                port: rows[0]?.port || DEFAULT_DAEMON_PORT,
                              },
                            ]);
                          }}
                        >
                          Add address
                        </Button>
                      </div>
                    </details>

                    <p className="text-xs text-ink/45">
                      Saves as <span className="font-mono text-ink/60">{daemonListen}</span>
                    </p>
                  </div>
                  <ToggleRow
                    id="daemon-auth"
                    label="Require operator auth"
                    description="When off, the control plane accepts unauthenticated requests."
                    checked={daemonAuthEnabled}
                    onChange={setDaemonAuthEnabled}
                  />
                </div>
              </section>

              <section className="space-y-3">
                <div className="space-y-1">
                  <h2 className="text-lg font-semibold text-ink">Library</h2>
                  <p className="text-sm text-ink/55">
                    Behavior for scheduled scans and pending titles.
                  </p>
                </div>
                <div className="divide-y divide-ink/10 bg-white/35 px-3">
                  <ToggleRow
                    id="auto-acquire"
                    label="Auto acquire"
                    description="Acquire pending titles after scans without a manual trigger."
                    checked={autoAcquire}
                    onChange={setAutoAcquire}
                  />
                </div>
              </section>

              <section className="space-y-5">
                <div className="space-y-1">
                  <h2 className="text-lg font-semibold text-ink">Plugins</h2>
                  <p className="text-sm text-ink/55">
                    Editable knobs exposed by loaded plugins. Empty groups have nothing to configure here.
                  </p>
                </div>

                {pluginsByKind.size === 0 ? (
                  <p className="text-sm text-ink/50">No plugins discovered.</p>
                ) : (
                  Array.from(pluginsByKind.entries()).map(([kind, plugins]) => (
                    <div key={kind} className="space-y-2">
                      <h3 className="text-sm font-semibold text-ink/70">{kindLabel(kind)}</h3>
                      <ul className="divide-y divide-ink/10 bg-white/35">
                        {plugins.map((plugin) => {
                          const rowKey = pluginRowKey(plugin);
                          const expanded = expandedPlugins.has(rowKey);
                          const enabledOption = findEnabledOption(plugin);
                          const enabled = enabledOption
                            ? parseBooleanLike(
                                pluginValues[enabledOption.key] ?? enabledOption.value,
                              )
                            : null;
                          const detailSettings = plugin.settings.filter(
                            (option) => option.key !== enabledOption?.key,
                          );
                          const canExpand =
                            detailSettings.length > 0 || plugin.settings.length === 0;

                          return (
                            <li key={rowKey} className="px-3 py-2.5">
                              <div className="flex flex-wrap items-center gap-2 sm:gap-3">
                                <button
                                  type="button"
                                  className="flex min-w-0 flex-1 items-center gap-2 rounded-md py-1 text-left hover:bg-ink/5 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal"
                                  aria-expanded={expanded}
                                  aria-controls={`plugin-settings-${rowKey}`}
                                  disabled={!canExpand}
                                  onClick={() => {
                                    setExpandedPlugins((current) => {
                                      const next = new Set(current);
                                      if (next.has(rowKey)) next.delete(rowKey);
                                      else next.add(rowKey);
                                      return next;
                                    });
                                  }}
                                >
                                  {canExpand ? (
                                    expanded ? (
                                      <ChevronDown className="h-4 w-4 shrink-0 text-ink/45" />
                                    ) : (
                                      <ChevronRight className="h-4 w-4 shrink-0 text-ink/45" />
                                    )
                                  ) : (
                                    <span className="inline-block h-4 w-4 shrink-0" />
                                  )}
                                  <PluginLogo plugin={plugin} />
                                  <span className="truncate font-medium text-ink">{plugin.id}</span>
                                  <span className="hidden text-xs text-ink/45 sm:inline">
                                    {plugin.kind}
                                  </span>
                                </button>

                                {enabled !== null && enabledOption ? (
                                  <div className="ml-auto flex items-center gap-2">
                                    <Badge
                                      className={
                                        enabled
                                          ? "bg-teal/15 text-ink normal-case tracking-normal"
                                          : "bg-ink/5 text-ink/55 normal-case tracking-normal"
                                      }
                                    >
                                      {enabled ? "Enabled" : "Disabled"}
                                    </Badge>
                                    <input
                                      id={`plugin-enabled-${rowKey}`}
                                      type="checkbox"
                                      className="h-4 w-4 accent-teal"
                                      aria-label={`${plugin.id} enabled`}
                                      checked={enabled}
                                      onChange={(e) =>
                                        setPluginValue(enabledOption, String(e.target.checked))
                                      }
                                      onClick={(e) => e.stopPropagation()}
                                    />
                                  </div>
                                ) : (
                                  <span className="ml-auto text-xs text-ink/45">{plugin.kind}</span>
                                )}
                              </div>

                              {expanded ? (
                                <div
                                  id={`plugin-settings-${rowKey}`}
                                  className="mt-3 border-t border-ink/10 pt-3"
                                >
                                  {detailSettings.length === 0 ? (
                                    <p className="text-xs text-ink/50">
                                      {plugin.settings.length === 0
                                        ? "No editable settings exposed for this plugin."
                                        : "No additional settings beyond Enabled."}
                                    </p>
                                  ) : (
                                    <div className="grid gap-3 sm:grid-cols-2">
                                      {detailSettings.map((option) => {
                                        const control = renderPluginOption(plugin, option);
                                        if (option.value_type === "boolean") {
                                          return (
                                            <div
                                              key={`${plugin.kind}-${plugin.id}-${option.key}`}
                                              className="border-t border-ink/10 sm:col-span-2"
                                            >
                                              {control}
                                            </div>
                                          );
                                        }
                                        return control;
                                      })}
                                    </div>
                                  )}
                                </div>
                              ) : null}
                            </li>
                          );
                        })}
                      </ul>
                    </div>
                  ))
                )}
              </section>
            </form>
          ) : (
            <ErrorStatePage
              title="Operator settings unavailable"
              message={
                operatorLoadError ??
                "Operator settings could not be loaded. Check your operator session, verify bookclerkd is running, then try Refresh."
              }
              onRetry={() => void refresh()}
            />
          )
        ) : null}
      </main>
      </div>

      {role === "operator" && settings && !loading ? (
        <div className="shrink-0 border-t border-ink/10 bg-paper/90 px-4 py-3 backdrop-blur-md">
          <div className={cn("flex flex-wrap items-center justify-between gap-3", pageWidthClass)}>
            <p className={`text-xs ${operatorHasValidationErrors ? "text-brick" : "text-ink/50"}`}>
              {operatorHasValidationErrors
                ? "Fix plugin field errors before saving."
                : operatorDirty
                  ? "You have unsaved changes."
                  : "No unsaved changes."}
            </p>
            <Button
              type="submit"
              form="operator-settings-form"
              disabled={saving || !operatorDirty || operatorHasValidationErrors}
            >
              {saving ? "Saving…" : "Save settings"}
            </Button>
          </div>
        </div>
      ) : null}

      {consentPrompt ? (
        <div
          className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 px-4 py-10 sm:items-center"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget && !consentBusy) {
              setConsentPrompt(null);
            }
          }}
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="plugin-consent-title"
            className="w-full max-w-lg rounded-lg border border-ink/10 bg-paper p-5 shadow-xl"
          >
            <h2
              id="plugin-consent-title"
              className="font-display text-xl font-semibold text-ink"
            >
              Approve plugin permissions
            </h2>
            <p className="mt-1 text-sm text-ink/55">
              Enabling <span className="font-medium text-ink">{consentPrompt.plugin_id}</span>{" "}
              requires consent for its network mode and host bindings. Workerd plugins enforce
              declared domains inside the isolate (redirect hops after an allowed initial host
              do not require re-approval). Native outbound is coarse jail internet with{" "}
              <span className="font-medium text-ink">no hostname allowlist</span>.
            </p>
            <ul className="mt-4 list-disc space-y-1 pl-5 text-sm text-ink/80">
              {consentPrompt.summary.map((line) => (
                <li key={line}>{line}</li>
              ))}
            </ul>
            <div className="mt-5 flex flex-wrap justify-end gap-2">
              <Button
                type="button"
                variant="ghost"
                disabled={consentBusy}
                onClick={() => setConsentPrompt(null)}
              >
                Cancel
              </Button>
              <Button type="button" disabled={consentBusy} onClick={() => void onConsentApprove()}>
                {consentBusy ? "Approving…" : "Approve and enable"}
              </Button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
