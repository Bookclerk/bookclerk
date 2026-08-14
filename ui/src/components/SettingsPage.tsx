import { useCallback, useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";
import { ChevronDown, ChevronRight, RefreshCw } from "lucide-react";
import { AccountSettingsPanel } from "@/components/AccountSettingsPanel";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import { ErrorStatePage } from "@/components/ErrorStatePage";
import { OidcSettingsPanel } from "@/components/OidcSettingsPanel";
import { UserManagementPanel } from "@/components/UserManagementPanel";
import {
  PluginConsentDialog,
  type PluginConsentGrantDraft,
} from "@/components/PluginConsentDialog";
import { CpuCoresSlider } from "@/components/CpuCoresSlider";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  approvePluginConsent,
  fetchPluginConsent,
  fetchSettings,
  isApiError,
  listSessions,
  listUsers,
  patchSettings,
  revokeSession,
  type AuthSession,
  type ListedUser,
  type ListedSession,
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
type SettingsTab = "account" | "users" | "signin" | "server" | "plugins";

const ISOLATION_OPTIONS = [
  ["required", "Required"],
  ["best-effort", "Best effort"],
  ["off", "Off"],
] as const;

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

function optionalIntegerError(value: string, label: string, min = 0, max?: number): string | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  if (!/^\d+$/.test(trimmed)) return `${label} must be a whole number`;
  const n = Number(trimmed);
  if (n < min) return `${label} must be at least ${min}`;
  if (max != null && n > max) return `${label} must be ${max} or lower`;
  return null;
}

const DEFAULT_JAIL_CPU_CORES = 0.8;
const DEFAULT_JAIL_EXTRA_PROCESSES = "2";

/** Format cores for the Settings patch body (daemon already validates/clamps). */
function coresSettingValue(cores: number | null | undefined): string {
  if (cores == null || !Number.isFinite(cores)) {
    return DEFAULT_JAIL_CPU_CORES.toFixed(2);
  }
  return cores.toFixed(2);
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

function runtimeLoadedKey(plugin: PluginSettingsGroup): string {
  const plural =
    plugin.kind === "source"
      ? "sources"
      : plugin.kind === "integration"
        ? "integrations"
        : `${plugin.kind}s`;
  return `runtime.${plural}.${plugin.id}.loaded`;
}

function effectiveLoaded(settings: SettingsResponse | null, plugin: PluginSettingsGroup): boolean {
  return settings?.effective?.[runtimeLoadedKey(plugin)] === "true";
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

/**
 * Operator/user Settings — daemon listen, plugins, consent, and user admin.
 *
 * @param props - Session hooks, nav, role, and optional session snapshot.
 */
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
  const [pluginsIsolation, setPluginsIsolation] = useState("required");
  const [mediaIsolation, setMediaIsolation] = useState("required");
  const [jailMemoryMiB, setJailMemoryMiB] = useState("");
  const [jailCpuCores, setJailCpuCores] = useState(DEFAULT_JAIL_CPU_CORES);
  const [jailExtraProcesses, setJailExtraProcesses] = useState(DEFAULT_JAIL_EXTRA_PROCESSES);
  const [pluginValues, setPluginValues] = useState<Record<string, string>>({});
  const [pluginErrors, setPluginErrors] = useState<Record<string, string>>({});
  const [consentCoverage, setConsentCoverage] = useState<Record<string, PluginConsentResponse>>({});
  /** Plugins start collapsed; keys are `${kind}:${id}`. */
  const [expandedPlugins, setExpandedPlugins] = useState<Set<string>>(() => new Set());
  const [consentPrompt, setConsentPrompt] = useState<PluginConsentResponse | null>(null);
  const [pendingEnableOption, setPendingEnableOption] = useState<PluginSettingOption | null>(null);
  const [consentBusy, setConsentBusy] = useState(false);
  const [operatorBaseline, setOperatorBaseline] = useState<{
    daemonListen: string;
    daemonAuthEnabled: boolean;
    autoAcquire: boolean;
    pluginsIsolation: string;
    mediaIsolation: string;
    jailMemoryMiB: string;
    jailCpuCores: number;
    jailExtraProcesses: string;
    pluginValues: Record<string, string>;
  } | null>(null);
  const [users, setUsers] = useState<ListedUser[]>([]);
  const [usersError, setUsersError] = useState<string | null>(null);
  const [usersBusy, setUsersBusy] = useState(false);
  const [sessions, setSessions] = useState<ListedSession[]>([]);
  const [sessionsBusy, setSessionsBusy] = useState(false);
  const [sessionsError, setSessionsError] = useState<string | null>(null);

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
  const jailMemoryError = optionalIntegerError(jailMemoryMiB, "Memory MiB");
  const hostCpuCoresMax =
    typeof settings?.host_cpu_cores_max === "number" &&
    Number.isFinite(settings.host_cpu_cores_max) &&
    settings.host_cpu_cores_max > 0
      ? settings.host_cpu_cores_max
      : 1;
  const jailCpuError =
    jailCpuCores < 0.01 || hostCpuCoresMax < jailCpuCores
      ? `CPU cores must be between 0.01 and ${hostCpuCoresMax.toFixed(2)}`
      : null;
  const jailProcessError = optionalIntegerError(
    jailExtraProcesses,
    "Additional processes",
    0,
    62,
  );
  const confinementHasErrors = Boolean(jailMemoryError || jailCpuError || jailProcessError);
  const isImpersonating = Boolean(session?.impersonating);
  const showOperatorChrome = role === "operator" && !isImpersonating;
  const showSignInSettings = (role === "operator" || role === "owner") && !isImpersonating;
  const canManageUsers = role === "operator" || role === "owner" || role === "administrator";
  const showUserAdmin = canManageUsers && (!isImpersonating || role === "administrator");
  const canManageOperator = showOperatorChrome;
  const showBootstrap = showOperatorChrome && !loading && users.length === 0;
  const adminCount = useMemo(
    () => users.filter((user) => user.role === "administrator" && user.status === "active").length,
    [users],
  );
  const currentUserId = session?.user?.id;

  function resolveDefaultTab(): SettingsTab {
    return "account";
  }

  const [activeTab, setActiveTab] = useState<SettingsTab>("account");
  const [tabInitialized, setTabInitialized] = useState(false);

  useEffect(() => {
    if (!loading && !tabInitialized) {
      setActiveTab(resolveDefaultTab());
      setTabInitialized(true);
    }
  }, [loading, tabInitialized]);

  useEffect(() => {
    if (!isImpersonating) return;
    if (activeTab === "server" || activeTab === "plugins") {
      setActiveTab("account");
      return;
    }
    // Impersonating a non-privileged user: hide User Management.
    if (activeTab === "users" && role !== "administrator" && role !== "owner") {
      setActiveTab("account");
    }
    if (activeTab === "signin") {
      setActiveTab("account");
    }
  }, [isImpersonating, activeTab, role]);

  useEffect(() => {
    if (!showUserAdmin && activeTab === "users") {
      setActiveTab("account");
    }
    if (!showSignInSettings && activeTab === "signin") {
      setActiveTab("account");
    }
    if (!showOperatorChrome && (activeTab === "server" || activeTab === "plugins")) {
      setActiveTab("account");
    }
  }, [showUserAdmin, showSignInSettings, showOperatorChrome, activeTab]);

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

  const prefetchConsentCoverage = useCallback(async (nextSettings: SettingsResponse) => {
    const entries = await Promise.all(
      nextSettings.plugins.map(async (plugin) => {
        try {
          return [plugin.id, await fetchPluginConsent(plugin.id)] as const;
        } catch {
          return null;
        }
      }),
    );
    setConsentCoverage(
      entries.reduce<Record<string, PluginConsentResponse>>((acc, entry) => {
        if (entry) acc[entry[0]] = entry[1];
        return acc;
      }, {}),
    );
  }, []);

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
    Object.keys(pluginErrors).length > 0 || daemonListenError !== null || confinementHasErrors;

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
    if (pluginsIsolation !== operatorBaseline.pluginsIsolation) {
      return true;
    }
    if (mediaIsolation !== operatorBaseline.mediaIsolation) {
      return true;
    }
    if (jailMemoryMiB !== operatorBaseline.jailMemoryMiB) {
      return true;
    }
    if (jailCpuCores !== operatorBaseline.jailCpuCores) {
      return true;
    }
    if (jailExtraProcesses !== operatorBaseline.jailExtraProcesses) {
      return true;
    }
    const currentKeys = Object.keys(pluginValues);
    const baselineKeys = Object.keys(operatorBaseline.pluginValues);
    if (currentKeys.length !== baselineKeys.length) {
      return true;
    }
    return currentKeys.some((key) => pluginValues[key] !== operatorBaseline.pluginValues[key]);
  }, [
    autoAcquire,
    daemonAuthEnabled,
    daemonListen,
    jailCpuCores,
    jailExtraProcesses,
    jailMemoryMiB,
    mediaIsolation,
    operatorBaseline,
    pluginValues,
    pluginsIsolation,
  ]);

  const refresh = useCallback(async () => {
    setError(null);
    setOperatorLoadError(null);
    setUsersError(null);
    setSessionsError(null);
    setLoading(true);
    try {
      if (!canManageOperator) {
        setSettings(null);
        setPluginValues({});
        setPluginErrors({});
        setConsentCoverage({});
        setOperatorBaseline(null);
      } else {
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
        setPluginsIsolation(nextSettings.settings["plugins.isolation"] ?? "required");
        setMediaIsolation(nextSettings.settings["media.isolation"] ?? "required");
        setJailMemoryMiB(nextSettings.settings["plugins.jail.memory_mib"] ?? "");
        setJailCpuCores(nextSettings.jail_cpu_cores ?? DEFAULT_JAIL_CPU_CORES);
        setJailExtraProcesses(
          nextSettings.settings["plugins.jail.extra_processes"] ?? DEFAULT_JAIL_EXTRA_PROCESSES,
        );
        setPluginValues(nextPluginValues);
        setPluginErrors({});
        setOperatorBaseline({
          daemonListen:
            exposure === "custom"
              ? joinListenRows(rows)
              : listenListFromExposure(exposure, rows[0]?.port ?? DEFAULT_DAEMON_PORT),
          daemonAuthEnabled: nextSettings.settings["daemon.auth.enabled"] === "true",
          autoAcquire: nextSettings.settings["library.auto_acquire"] === "true",
          pluginsIsolation: nextSettings.settings["plugins.isolation"] ?? "required",
          mediaIsolation: nextSettings.settings["media.isolation"] ?? "required",
          jailMemoryMiB: nextSettings.settings["plugins.jail.memory_mib"] ?? "",
          jailCpuCores: nextSettings.jail_cpu_cores ?? DEFAULT_JAIL_CPU_CORES,
          jailExtraProcesses:
            nextSettings.settings["plugins.jail.extra_processes"] ?? DEFAULT_JAIL_EXTRA_PROCESSES,
          pluginValues: nextPluginValues,
        });
        void prefetchConsentCoverage(nextSettings);
      }

      if (showUserAdmin) {
        try {
          setUsers(await listUsers());
        } catch (err) {
          setUsers([]);
          setUsersError(err instanceof Error ? err.message : "Failed to load users");
        }
      } else {
        setUsers([]);
      }

      try {
        setSessions(await listSessions());
      } catch (err) {
        setSessions([]);
        setSessionsError(err instanceof Error ? err.message : "Failed to load sessions");
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
  }, [canManageOperator, onSessionExpired, prefetchConsentCoverage, showUserAdmin]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function saveOperatorSettings(): Promise<SettingsResponse> {
    const pluginUpdates = Object.entries(pluginValues).map(([key, value]) => ({ key, value }));
    const nextListen = daemonListen;
    return withRequestTimeout(
      patchSettings({
        settings: [
          { key: "daemon.listen", value: nextListen },
          { key: "daemon.auth.enabled", value: String(daemonAuthEnabled) },
          { key: "library.auto_acquire", value: String(autoAcquire) },
          { key: "plugins.isolation", value: pluginsIsolation },
          { key: "media.isolation", value: mediaIsolation },
          { key: "plugins.jail.memory_mib", value: jailMemoryMiB.trim() },
          { key: "plugins.jail.cpu_cores", value: coresSettingValue(jailCpuCores) },
          {
            key: "plugins.jail.extra_processes",
            value: jailExtraProcesses.trim() || DEFAULT_JAIL_EXTRA_PROCESSES,
          },
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
    setPluginsIsolation(next.settings["plugins.isolation"] ?? "required");
    setMediaIsolation(next.settings["media.isolation"] ?? "required");
    setJailMemoryMiB(next.settings["plugins.jail.memory_mib"] ?? "");
    setJailCpuCores(next.jail_cpu_cores ?? DEFAULT_JAIL_CPU_CORES);
    setJailExtraProcesses(
      next.settings["plugins.jail.extra_processes"] ?? DEFAULT_JAIL_EXTRA_PROCESSES,
    );
    setPluginValues(nextPluginValues);
    setPluginErrors({});
    setOperatorBaseline({
      daemonListen:
        exposure === "custom"
          ? joinListenRows(rows)
          : listenListFromExposure(exposure, rows[0]?.port ?? DEFAULT_DAEMON_PORT),
      daemonAuthEnabled: next.settings["daemon.auth.enabled"] === "true",
      autoAcquire: next.settings["library.auto_acquire"] === "true",
      pluginsIsolation: next.settings["plugins.isolation"] ?? "required",
      mediaIsolation: next.settings["media.isolation"] ?? "required",
      jailMemoryMiB: next.settings["plugins.jail.memory_mib"] ?? "",
      jailCpuCores: next.jail_cpu_cores ?? DEFAULT_JAIL_CPU_CORES,
      jailExtraProcesses:
        next.settings["plugins.jail.extra_processes"] ?? DEFAULT_JAIL_EXTRA_PROCESSES,
      pluginValues: nextPluginValues,
    });
    void prefetchConsentCoverage(next);
  }

  async function promptConsentForPlugin(
    pluginId: string,
    fallbackSummary?: string[],
    enableOption?: PluginSettingOption,
  ) {
    setPendingEnableOption(enableOption ?? null);
    try {
      const consent = await fetchPluginConsent(pluginId);
      setConsentPrompt(consent);
    } catch {
      setConsentPrompt({
        plugin_id: pluginId,
        runtime: "native",
        request: {
          pluginId,
          kind: "",
          networkMode: "deny",
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
              "Approve network mode, bindings, and disk budget before enabling.",
              "Workerd guests also enforce domain allowlists and isolate CPU/subrequest budgets.",
              "Native guests use OS-jail allow-or-deny for network (no hostname filter).",
            ],
        limits: {
          cpu_ms: 30000,
          subrequests: 50,
          max_cpu_ms: 120000,
          max_subrequests: 1000,
          disk_mib: 512,
          max_disk_mib: 4096,
          memory_mib: 512,
          max_memory_mib: 4096,
          cpu_cores: 0.8,
          max_cpu_cores: 1,
          jail_cpu_cores: 0.8,
          extra_processes: 2,
          max_extra_processes: 62,
          known_bindings: ["config", "secrets", "plugin_kv", "work_fs", "oauth"],
        },
      });
    }
  }

  async function onPluginEnabledChange(
    plugin: PluginSettingsGroup,
    option: PluginSettingOption,
    checked: boolean,
  ) {
    if (!checked) {
      setPluginValue(option, "false");
      return;
    }
    setError(null);
    setPluginErrors((current) => {
      const { [option.key]: _removed, ...rest } = current;
      return rest;
    });
    try {
      const consent = consentCoverage[plugin.id] ?? (await fetchPluginConsent(plugin.id));
      setConsentCoverage((current) => ({ ...current, [plugin.id]: consent }));
      if (consent.covered) {
        setPluginValue(option, "true");
        return;
      }
      setPendingEnableOption(option);
      setConsentPrompt(consent);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load plugin consent");
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

  async function onConsentApprove(grant: PluginConsentGrantDraft) {
    if (!consentPrompt) return;
    setConsentBusy(true);
    setError(null);
    try {
      const approved = await approvePluginConsent(consentPrompt.plugin_id, grant);
      setConsentCoverage((current) => ({
        ...current,
        [consentPrompt.plugin_id]: approved,
      }));
      if (pendingEnableOption) {
        setPluginValue(pendingEnableOption, "true");
      }
      setConsentPrompt(null);
      setPendingEnableOption(null);
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

  async function reloadUsers() {
    if (!showUserAdmin) return;
    setUsersError(null);
    setUsers(await listUsers());
  }

  async function reloadSessions() {
    setSessionsError(null);
    setSessions(await listSessions());
  }

  async function onRevokeSession(id: number) {
    setSessionsBusy(true);
    setSessionsError(null);
    try {
      await revokeSession(id);
      await reloadSessions();
      await onSessionChange?.();
    } catch (err) {
      setSessionsError(err instanceof Error ? err.message : "Session revoke failed");
    } finally {
      setSessionsBusy(false);
    }
  }

  async function onRevokeOtherSessions() {
    const others = sessions.filter((row) => !row.is_current);
    if (others.length === 0) return;
    setSessionsBusy(true);
    setSessionsError(null);
    try {
      for (const row of others) {
        await revokeSession(row.id);
      }
      await reloadSessions();
      await onSessionChange?.();
    } catch (err) {
      setSessionsError(err instanceof Error ? err.message : "Session revoke failed");
    } finally {
      setSessionsBusy(false);
    }
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
              showOperatorChrome ? (
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
        <div className="space-y-3">
          <div className="space-y-1">
            <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">Settings</h1>
            <p className="text-sm text-ink/60">
              {showOperatorChrome
                ? "Account, users, sign-in providers, daemon, and plugin knobs for this host."
                : showSignInSettings
                  ? "Account security, user management, and sign-in providers."
                  : showUserAdmin
                    ? "Account security and user management. Discover preferences stay in the header Preferences dialog."
                    : "Account security and sessions. Discover preferences stay in the header Preferences dialog."}
            </p>
          </div>

          <div
            className="flex flex-wrap gap-1 rounded-md border border-ink/10 bg-white/40 p-1"
            role="tablist"
            aria-label="Settings sections"
          >
            <button
              type="button"
              role="tab"
              aria-selected={activeTab === "account"}
              className={cn(
                "rounded px-3 py-1.5 text-sm font-medium transition-colors",
                activeTab === "account"
                  ? "bg-ink text-paper shadow-sm"
                  : "text-ink/60 hover:text-ink",
              )}
              onClick={() => setActiveTab("account")}
            >
              Account
            </button>
            {showUserAdmin ? (
              <button
                type="button"
                role="tab"
                aria-selected={activeTab === "users"}
                className={cn(
                  "rounded px-3 py-1.5 text-sm font-medium transition-colors",
                  activeTab === "users"
                    ? "bg-ink text-paper shadow-sm"
                    : "text-ink/60 hover:text-ink",
                )}
                onClick={() => setActiveTab("users")}
              >
                User Management
              </button>
            ) : null}
            {showSignInSettings ? (
              <button
                type="button"
                role="tab"
                aria-selected={activeTab === "signin"}
                className={cn(
                  "rounded px-3 py-1.5 text-sm font-medium transition-colors",
                  activeTab === "signin"
                    ? "bg-ink text-paper shadow-sm"
                    : "text-ink/60 hover:text-ink",
                )}
                onClick={() => setActiveTab("signin")}
              >
                Sign-in
              </button>
            ) : null}
            {showOperatorChrome ? (
              <>
                <button
                  type="button"
                  role="tab"
                  aria-selected={activeTab === "server"}
                  className={cn(
                    "rounded px-3 py-1.5 text-sm font-medium transition-colors",
                    activeTab === "server"
                      ? "bg-ink text-paper shadow-sm"
                      : "text-ink/60 hover:text-ink",
                  )}
                  onClick={() => setActiveTab("server")}
                >
                  Server Settings
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={activeTab === "plugins"}
                  className={cn(
                    "rounded px-3 py-1.5 text-sm font-medium transition-colors",
                    activeTab === "plugins"
                      ? "bg-ink text-paper shadow-sm"
                      : "text-ink/60 hover:text-ink",
                  )}
                  onClick={() => setActiveTab("plugins")}
                >
                  Plugins
                </button>
              </>
            ) : null}
          </div>
        </div>

        {error && !showOperatorChrome ? (
          <ErrorStatePage
            title="Settings request failed"
            message={error}
            onRetry={() => void refresh()}
          />
        ) : null}

        {activeTab === "account" ? (
          <AccountSettingsPanel
            session={session ?? null}
            onSessionChange={onSessionChange}
            onDeleted={async () => {
              await onSignOut();
            }}
            sessions={sessions}
            sessionsBusy={sessionsBusy}
            sessionsError={sessionsError}
            onRefreshSessions={() => {
              void (async () => {
                setSessionsBusy(true);
                try {
                  await reloadSessions();
                } catch (err) {
                  setSessionsError(
                    err instanceof Error ? err.message : "Failed to load sessions",
                  );
                } finally {
                  setSessionsBusy(false);
                }
              })();
            }}
            onRevokeSession={(id) => void onRevokeSession(id)}
            onRevokeOtherSessions={() => void onRevokeOtherSessions()}
          />
        ) : null}

        {activeTab === "users" && showUserAdmin ? (
          <UserManagementPanel
            users={users}
            setUsers={setUsers}
            busy={usersBusy}
            setBusy={setUsersBusy}
            error={usersError}
            setError={setUsersError}
            showBootstrap={showBootstrap}
            showOperatorChrome={showOperatorChrome}
            session={session ?? null}
            adminCount={adminCount}
            currentUserId={currentUserId}
            onSessionChange={onSessionChange}
            onUsersChanged={async () => {
              await reloadUsers();
            }}
          />
        ) : null}

        {activeTab === "signin" && showSignInSettings ? <OidcSettingsPanel /> : null}

        {(activeTab === "server" || activeTab === "plugins") && showOperatorChrome ? (
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

              {activeTab === "server" ? (
                <>
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

              <section className="space-y-3">
                <div className="space-y-1">
                  <h2 className="text-lg font-semibold text-ink">Confinement</h2>
                  <p className="text-sm text-ink/55">
                    Host isolation policy for plugins and media workers.
                  </p>
                </div>
                <div className="grid gap-4 bg-white/35 px-3 py-3 sm:grid-cols-2">
                  <FieldBlock label="Plugin isolation" htmlFor="plugins-isolation">
                    <select
                      id="plugins-isolation"
                      value={pluginsIsolation}
                      onChange={(e) => setPluginsIsolation(e.target.value)}
                      className={selectClassName}
                    >
                      {ISOLATION_OPTIONS.map(([value, label]) => (
                        <option key={value} value={value}>
                          {label}
                        </option>
                      ))}
                    </select>
                  </FieldBlock>
                  <FieldBlock label="Media isolation" htmlFor="media-isolation">
                    <select
                      id="media-isolation"
                      value={mediaIsolation}
                      onChange={(e) => setMediaIsolation(e.target.value)}
                      className={selectClassName}
                    >
                      {ISOLATION_OPTIONS.map(([value, label]) => (
                        <option key={value} value={value}>
                          {label}
                        </option>
                      ))}
                    </select>
                  </FieldBlock>
                  <FieldBlock
                    label="Jail memory MiB"
                    htmlFor="plugins-jail-memory"
                    hint="Leave empty for the platform default."
                    error={jailMemoryError ?? undefined}
                  >
                    <Input
                      id="plugins-jail-memory"
                      type="number"
                      min={0}
                      value={jailMemoryMiB}
                      onChange={(e) => setJailMemoryMiB(e.target.value)}
                      placeholder="default"
                    />
                  </FieldBlock>
                  <FieldBlock
                    label="Jail CPU cores"
                    htmlFor="plugins-jail-cpu"
                    hint="Per-jail ceiling in cores (two decimals; 1.00 = one logical CPU). Defaults to 0.80. Idle guests do not reserve CPU."
                    error={jailCpuError ?? undefined}
                  >
                    <div className="flex flex-col gap-2">
                      <CpuCoresSlider
                        id="plugins-jail-cpu"
                        value={jailCpuCores}
                        onChange={(cores) => setJailCpuCores(cores)}
                        hostMaxCores={hostCpuCoresMax}
                        disabled={false}
                      />
                      <Button
                        type="button"
                        variant="ghost"
                        className="self-start px-0 text-xs text-ink/55"
                        onClick={() => setJailCpuCores(DEFAULT_JAIL_CPU_CORES)}
                      >
                        Reset to default (0.80)
                      </Button>
                    </div>
                  </FieldBlock>
                  <FieldBlock
                    label="Additional processes"
                    htmlFor="plugins-jail-processes"
                    hint="Ceiling on extra processes/threads beyond each guest’s launcher overhead (native default 2; workerd headroom is host-managed)."
                    error={jailProcessError ?? undefined}
                  >
                    <Input
                      id="plugins-jail-processes"
                      type="number"
                      min={0}
                      max={62}
                      value={jailExtraProcesses}
                      onChange={(e) => setJailExtraProcesses(e.target.value)}
                      placeholder={DEFAULT_JAIL_EXTRA_PROCESSES}
                    />
                  </FieldBlock>
                </div>
              </section>
                </>
              ) : null}

              {activeTab === "plugins" ? (
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
                          const consent = consentCoverage[plugin.id];
                          const loaded = effectiveLoaded(settings, plugin);

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
                                    {loaded ? (
                                      <Badge className="bg-teal/10 text-ink/70 normal-case tracking-normal">
                                        Loaded
                                      </Badge>
                                    ) : null}
                                    {consent ? (
                                      <Badge
                                        className={
                                          consent.covered
                                            ? "bg-teal/15 text-ink normal-case tracking-normal"
                                            : "bg-brick/10 text-brick normal-case tracking-normal"
                                        }
                                      >
                                        {consent.covered ? "Granted" : "Needs approval"}
                                      </Badge>
                                    ) : null}
                                    <input
                                      id={`plugin-enabled-${rowKey}`}
                                      type="checkbox"
                                      className="h-4 w-4 accent-teal"
                                      aria-label={`${plugin.id} enabled`}
                                      checked={enabled}
                                      onChange={(e) =>
                                        void onPluginEnabledChange(
                                          plugin,
                                          enabledOption,
                                          e.target.checked,
                                        )
                                      }
                                      onClick={(e) => e.stopPropagation()}
                                    />
                                  </div>
                                ) : (
                                  <div className="ml-auto flex items-center gap-2">
                                    {loaded ? (
                                      <Badge className="bg-teal/10 text-ink/70 normal-case tracking-normal">
                                        Loaded
                                      </Badge>
                                    ) : null}
                                    {consent ? (
                                      <Badge
                                        className={
                                          consent.covered
                                            ? "bg-teal/15 text-ink normal-case tracking-normal"
                                            : "bg-brick/10 text-brick normal-case tracking-normal"
                                        }
                                      >
                                        {consent.covered ? "Granted" : "Needs approval"}
                                      </Badge>
                                    ) : null}
                                    <span className="text-xs text-ink/45">{plugin.kind}</span>
                                  </div>
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
              ) : null}
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

      {showOperatorChrome && settings && !loading && (activeTab === "server" || activeTab === "plugins") ? (
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
        <PluginConsentDialog
          consent={consentPrompt}
          busy={consentBusy}
          onCancel={() => {
            setConsentPrompt(null);
            setPendingEnableOption(null);
          }}
          onApprove={(grant) => void onConsentApprove(grant)}
        />
      ) : null}
    </div>
  );
}
