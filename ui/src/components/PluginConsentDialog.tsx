import { useEffect, useId, useMemo, useState, type CSSProperties, type ReactNode } from "react";
import { Check, ChevronDown, Globe, HardDrive, Layers, Shield, X } from "lucide-react";
import { CpuCoresSlider } from "@/components/CpuCoresSlider";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { PluginConsentResponse } from "@/lib/api";
import { cn } from "@/lib/utils";

export type PluginConsentGrantDraft = {
  networkMode: string;
  domains: string[];
  bindings: string[];
  compatibilityFlags: string[];
  cpuMs?: number;
  subrequests?: number;
  diskMib?: number;
  memoryMib?: number;
  cpuCores?: number;
  extraProcesses?: number;
};

type SectionId = "network" | "bindings" | "resources" | "workerd" | "flags";

function uniqueValues(values: string[] | undefined): string[] {
  const out: string[] = [];
  for (const raw of values ?? []) {
    const value = raw.trim();
    if (!value) continue;
    if (!out.some((existing) => existing.toLowerCase() === value.toLowerCase())) {
      out.push(value);
    }
  }
  return out;
}

function parsePositiveInt(value: string, max?: number): number | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  const parsed = Number.parseInt(trimmed, 10);
  if (!Number.isFinite(parsed) || parsed < 1) return undefined;
  return max == null ? parsed : Math.min(parsed, max);
}

function parseNonNegInt(value: string, max?: number): number | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  const parsed = Number.parseInt(trimmed, 10);
  if (!Number.isFinite(parsed) || parsed < 0) return undefined;
  return max == null ? parsed : Math.min(parsed, max);
}

function initialList(requested: string[], existing?: string[]): string[] {
  if (existing && existing.length > 0) return uniqueValues(existing);
  if (existing) return [];
  return uniqueValues(requested);
}

function formatCores(value: number): string {
  return (Number.isFinite(value) ? value : 0).toFixed(2);
}

function joinList(values: string[], empty = "none"): string {
  if (values.length === 0) return empty;
  if (values.length === 1) return values[0]!;
  if (values.length === 2) return `${values[0]} and ${values[1]}`;
  return `${values.slice(0, -1).join(", ")}, and ${values[values.length - 1]}`;
}

function ConsentSection({
  id,
  title,
  summary,
  preview,
  icon,
  open,
  onToggle,
  children,
}: {
  id: string;
  title: string;
  summary: string;
  /** Extra detail shown under the summary while collapsed (e.g. domain list). */
  preview?: ReactNode;
  icon: ReactNode;
  open: boolean;
  onToggle: () => void;
  children: ReactNode;
}) {
  const panelId = `${id}-panel`;
  return (
    <div className="border-b border-ink/10 last:border-b-0">
      <button
        type="button"
        id={id}
        aria-expanded={open}
        aria-controls={panelId}
        className="flex w-full items-start gap-3 py-3.5 text-left transition-colors hover:bg-ink/[0.03] focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-teal"
        onClick={onToggle}
      >
        <ChevronDown
          className={cn(
            "mt-0.5 h-4 w-4 shrink-0 text-ink/45 transition-transform duration-200",
            open ? "rotate-0" : "-rotate-90",
          )}
          aria-hidden
        />
        <span className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center text-teal" aria-hidden>
          {icon}
        </span>
        <span className="min-w-0 flex-1">
          <span className="block text-sm font-semibold text-ink">{title}</span>
          <span className="mt-0.5 block text-sm leading-snug text-ink/55">{summary}</span>
          {!open && preview ? <div className="mt-2">{preview}</div> : null}
        </span>
      </button>
      <div
        id={panelId}
        role="region"
        aria-labelledby={id}
        className={cn(
          "grid transition-[grid-template-rows,opacity] duration-200 ease-out",
          open ? "grid-rows-[1fr] opacity-100" : "grid-rows-[0fr] opacity-0",
        )}
      >
        <div className="overflow-hidden">
          <div className="space-y-3 pb-4 pl-12 pr-1">{children}</div>
        </div>
      </div>
    </div>
  );
}

/**
 * Branded plugin consent dialog in an OAuth-style grant review layout.
 *
 * Default view lists clear permission summaries. Each section expands for
 * advanced operator configuration. Host hard caps still apply.
 *
 * @param props - Consent payload, busy state, close handler, and approve callback.
 */
export function PluginConsentDialog({
  consent,
  busy,
  onCancel,
  onApprove,
}: {
  consent: PluginConsentResponse;
  busy: boolean;
  onCancel: () => void;
  onApprove: (grant: PluginConsentGrantDraft) => void | Promise<void>;
}) {
  const request = consent.request;
  const existing = consent.existing;
  const brand = consent.brand;
  const limits = consent.limits;
  const isWorkerd = consent.runtime === "workerd";
  const brandName = brand?.name?.trim() || request.pluginId || consent.plugin_id;
  const knownBindings = uniqueValues([
    ...(limits.known_bindings ?? []),
    ...request.bindings,
    ...(existing?.bindings ?? []),
  ]);
  const titleId = useId();
  /** Exclusive accordion — one advanced section at a time, OAuth-style. */
  const [openSection, setOpenSection] = useState<SectionId | null>(null);
  const [networkMode, setNetworkMode] = useState(request.networkMode || "deny");
  const [domains, setDomains] = useState<string[]>([]);
  const [domainDraft, setDomainDraft] = useState("");
  const [bindings, setBindings] = useState<string[]>([]);
  const [compatibilityFlags, setCompatibilityFlags] = useState<string[]>([]);
  const [flagDraft, setFlagDraft] = useState("");
  const [cpuMs, setCpuMs] = useState("");
  const [subrequests, setSubrequests] = useState("");
  const [diskMib, setDiskMib] = useState("");
  const [memoryMib, setMemoryMib] = useState("");
  const [cpuCores, setCpuCores] = useState(limits.cpu_cores ?? 0.8);
  const [extraProcesses, setExtraProcesses] = useState(
    String(limits.extra_processes ?? 2),
  );

  useEffect(() => {
    setNetworkMode(existing?.networkMode || request.networkMode || "deny");
    setDomains(initialList(request.domains, existing?.domains));
    setBindings(initialList(request.bindings, existing?.bindings));
    setCompatibilityFlags(
      initialList(request.compatibilityFlags, existing?.compatibilityFlags),
    );
    setCpuMs(String(existing?.cpuMs ?? request.cpuMs ?? limits.cpu_ms ?? ""));
    setSubrequests(
      String(existing?.subrequests ?? request.subrequests ?? limits.subrequests ?? ""),
    );
    setDiskMib(String(existing?.diskMib ?? request.diskMib ?? limits.disk_mib ?? ""));
    setMemoryMib(
      String(existing?.memoryMib ?? request.memoryMib ?? limits.memory_mib ?? ""),
    );
    setCpuCores(existing?.cpuCores ?? request.cpuCores ?? limits.cpu_cores ?? 0.8);
    setExtraProcesses(
      String(
        existing?.extraProcesses ??
          request.extraProcesses ??
          limits.extra_processes ??
          2,
      ),
    );
    setDomainDraft("");
    setFlagDraft("");
    setOpenSection(null);
  }, [consent, existing, request, limits]);

  const selectedDomains = useMemo(() => uniqueValues(domains), [domains]);
  const suggestedDomains = useMemo(
    () => uniqueValues([...request.domains, ...selectedDomains]),
    [request.domains, selectedDomains],
  );

  const networkSummary = useMemo(() => {
    if (networkMode === "deny") return "No network access";
    if (!isWorkerd) return "Outbound network (OS jail allow-or-deny)";
    if (selectedDomains.length === 0) {
      return "Outbound network with no domains selected";
    }
    return "Outbound network to these domains";
  }, [isWorkerd, networkMode, selectedDomains.length]);

  const networkDomainPreview =
    isWorkerd && networkMode !== "deny" && selectedDomains.length > 0 ? (
      <ul className="space-y-1 text-sm text-ink/70">
        {selectedDomains.map((domain) => (
          <li key={domain} className="font-mono text-[13px] leading-snug">
            {domain}
          </li>
        ))}
      </ul>
    ) : null;

  const bindingsSummary = useMemo(() => {
    if (bindings.length === 0) return "No host bindings";
    return `Access ${joinList(bindings)}`;
  }, [bindings]);

  const resourcesSummary = useMemo(() => {
    const memory = parsePositiveInt(memoryMib, limits.max_memory_mib) ?? limits.memory_mib;
    const disk = parsePositiveInt(diskMib, limits.max_disk_mib) ?? limits.disk_mib;
    const parts = [`${memory} MiB memory`, `${disk} MiB disk`];
    if (!isWorkerd) {
      const extra =
        parseNonNegInt(extraProcesses, limits.max_extra_processes) ??
        limits.extra_processes;
      parts.splice(
        1,
        0,
        `${formatCores(cpuCores)} CPU cores`,
        `${extra} additional processes/threads`,
      );
    }
    return `Use up to ${joinList(parts)}`;
  }, [
    cpuCores,
    diskMib,
    extraProcesses,
    isWorkerd,
    limits.disk_mib,
    limits.extra_processes,
    limits.max_disk_mib,
    limits.max_extra_processes,
    limits.memory_mib,
    limits.max_memory_mib,
    memoryMib,
  ]);

  const workerdSummary = useMemo(() => {
    const cpu = parsePositiveInt(cpuMs, limits.max_cpu_ms) ?? limits.cpu_ms;
    const subs =
      parsePositiveInt(subrequests, limits.max_subrequests) ?? limits.subrequests;
    return `Isolate budget of ${cpu.toLocaleString()} ms CPU and ${subs.toLocaleString()} subrequests`;
  }, [cpuMs, limits.cpu_ms, limits.max_cpu_ms, limits.max_subrequests, limits.subrequests, subrequests]);

  const flagsSummary = useMemo(() => {
    if (compatibilityFlags.length === 0) return "No compatibility flags";
    if (compatibilityFlags.length <= 2) return joinList(compatibilityFlags);
    return `${compatibilityFlags.length} compatibility flags`;
  }, [compatibilityFlags]);

  function toggleSection(id: SectionId) {
    setOpenSection((current) => (current === id ? null : id));
  }

  function toggleValue(
    value: string,
    checked: boolean,
    setter: (next: string[]) => void,
    current: string[],
  ) {
    setter(
      checked
        ? uniqueValues([...current, value])
        : current.filter((item) => item.toLowerCase() !== value.toLowerCase()),
    );
  }

  function addDomain() {
    const value = domainDraft.trim();
    if (!value) return;
    setDomains((current) => uniqueValues([...current, value]));
    setDomainDraft("");
  }

  function addFlag() {
    const value = flagDraft.trim();
    if (!value) return;
    setCompatibilityFlags((current) => uniqueValues([...current, value]));
    setFlagDraft("");
  }

  const accentStyle: CSSProperties = {
    background: brand?.accent || brand?.bg || undefined,
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/45 px-4 py-8 sm:items-center sm:py-10"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !busy) onCancel();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className="consent-dialog-enter w-full max-w-md overflow-hidden rounded-xl border border-ink/10 bg-white shadow-xl outline-none"
      >
        <div className="h-1 bg-teal" style={accentStyle} />

        <div className="relative px-6 pt-5 pb-2">
          <Button
            type="button"
            variant="ghost"
            className="absolute right-3 top-3 h-8 w-8 shrink-0 px-0"
            disabled={busy}
            onClick={onCancel}
            aria-label="Close plugin consent"
          >
            <X className="h-4 w-4" />
          </Button>

          <div className="flex items-center gap-3 pr-8">
            {brand?.logo ? (
              <img
                src={brand.logo}
                alt=""
                className="h-12 w-12 shrink-0 rounded-full border border-ink/10 bg-paper object-contain p-1.5"
              />
            ) : (
              <div
                className="flex h-12 w-12 shrink-0 items-center justify-center rounded-full text-sm font-semibold uppercase text-paper"
                style={{ background: brand?.bg || "var(--color-ink)" }}
              >
                {brandName.slice(0, 2)}
              </div>
            )}
            <div className="min-w-0">
              <p className="truncate text-base font-semibold text-ink">{brandName}</p>
              <p className="truncate text-xs text-ink/50">
                {[request.kind || "plugin", isWorkerd ? "workerd" : "native"]
                  .filter(Boolean)
                  .join(" · ")}
              </p>
            </div>
          </div>

          <h2
            id={titleId}
            className="mt-5 font-display text-2xl font-semibold tracking-tight text-ink"
          >
            Permissions requested
          </h2>
          <p className="mt-1.5 text-sm text-ink/60">
            Review the requested grants, then accept to enable. Expand a section to
            adjust advanced configuration.
          </p>
        </div>

        <div className="max-h-[min(58vh,28rem)] overflow-y-auto px-6">
          <p className="pb-1 pt-3 text-[11px] font-semibold uppercase tracking-[0.08em] text-ink/45">
            This plugin would like to
          </p>

          <ConsentSection
            id="consent-network"
            title="Network access"
            summary={networkSummary}
            preview={networkDomainPreview}
            icon={networkMode === "deny" ? <Shield className="h-4 w-4" /> : <Globe className="h-4 w-4" />}
            open={openSection === "network"}
            onToggle={() => toggleSection("network")}
          >
            <p className="text-xs text-ink/55">
              {isWorkerd
                ? "Workerd guests enforce deny/outbound plus the domain allowlist."
                : "Native guests use OS-jail allow-or-deny for network (no hostname filter)."}
            </p>
            <select
              className="w-full rounded-md border border-ink/15 bg-white px-3 py-2 text-sm text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
              value={networkMode}
              disabled={busy}
              onChange={(e) => setNetworkMode(e.target.value)}
            >
              <option value="outbound">Allow outbound network</option>
              <option value="deny">Deny network</option>
            </select>
            {isWorkerd ? (
              <div className="space-y-2">
                <p className="text-xs font-medium text-ink/70">Allowed domains</p>
                <div className="flex flex-wrap gap-1.5">
                  {suggestedDomains.map((domain) => {
                    const checked = selectedDomains.some(
                      (item) => item.toLowerCase() === domain.toLowerCase(),
                    );
                    const extra = !request.domains.some(
                      (item) => item.toLowerCase() === domain.toLowerCase(),
                    );
                    return (
                      <button
                        key={domain}
                        type="button"
                        disabled={busy || networkMode === "deny"}
                        className={cn(
                          "rounded-md border px-2 py-1 text-xs font-medium transition-colors",
                          checked
                            ? "border-teal/40 bg-teal/15 text-ink"
                            : "border-ink/10 bg-paper/80 text-ink/55",
                        )}
                        onClick={() =>
                          setDomains((current) =>
                            checked
                              ? current.filter(
                                  (item) => item.toLowerCase() !== domain.toLowerCase(),
                                )
                              : uniqueValues([...current, domain]),
                          )
                        }
                      >
                        {domain}
                        {extra ? " +" : ""}
                      </button>
                    );
                  })}
                </div>
                <div className="flex gap-2">
                  <Input
                    value={domainDraft}
                    disabled={busy || networkMode === "deny"}
                    onChange={(e) => setDomainDraft(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        e.preventDefault();
                        addDomain();
                      }
                    }}
                    placeholder="Add domain"
                    autoComplete="off"
                    spellCheck={false}
                  />
                  <Button
                    type="button"
                    variant="secondary"
                    disabled={busy || networkMode === "deny" || !domainDraft.trim()}
                    onClick={addDomain}
                  >
                    Add
                  </Button>
                </div>
              </div>
            ) : null}
          </ConsentSection>

          <ConsentSection
            id="consent-bindings"
            title="Host bindings"
            summary={bindingsSummary}
            icon={<Layers className="h-4 w-4" />}
            open={openSection === "bindings"}
            onToggle={() => toggleSection("bindings")}
          >
            <p className="text-xs text-ink/55">
              Shared by native and workerd guests. Grant or remove bindings relative
              to the plugin request.
            </p>
            <ul className="space-y-2">
              {knownBindings.map((binding) => {
                const requested = request.bindings.some(
                  (item) => item.toLowerCase() === binding.toLowerCase(),
                );
                const checked = bindings.some(
                  (item) => item.toLowerCase() === binding.toLowerCase(),
                );
                return (
                  <li key={binding}>
                    <label className="flex cursor-pointer items-center gap-2.5 text-sm text-ink">
                      <input
                        type="checkbox"
                        className="h-4 w-4 accent-teal"
                        checked={checked}
                        disabled={busy}
                        onChange={(e) =>
                          toggleValue(binding, e.target.checked, setBindings, bindings)
                        }
                      />
                      <span className="flex items-center gap-1.5">
                        {checked ? (
                          <Check className="h-3.5 w-3.5 text-teal" aria-hidden />
                        ) : null}
                        <span>{binding}</span>
                        {!requested ? (
                          <span className="text-xs text-ink/40">extra</span>
                        ) : null}
                      </span>
                    </label>
                  </li>
                );
              })}
            </ul>
          </ConsentSection>

          <ConsentSection
            id="consent-resources"
            title="Jail resources"
            summary={resourcesSummary}
            icon={<HardDrive className="h-4 w-4" />}
            open={openSection === "resources"}
            onToggle={() => toggleSection("resources")}
          >
            <p className="text-xs text-ink/55">
              {isWorkerd
                ? "Memory and disk apply via the OS jail. Process headroom is host-managed. Isolate CPU is configured separately."
                : "Applied via the OS jail. CPU is cores (1.00 = one logical CPU). Additional processes/threads are beyond the main guest (Linux counts threads). Disk budgets cover each of data/ and tmp/."}
            </p>
            <div className="grid gap-3 sm:grid-cols-2">
              <label className="space-y-1.5 text-sm font-medium text-ink">
                Memory MiB
                <Input
                  type="number"
                  min={1}
                  max={limits.max_memory_mib}
                  value={memoryMib}
                  disabled={busy}
                  onChange={(e) => setMemoryMib(e.target.value)}
                />
                <span className="block text-xs font-normal text-ink/50">
                  Max {limits.max_memory_mib}
                </span>
              </label>
              {!isWorkerd ? (
                <label className="space-y-1.5 text-sm font-medium text-ink">
                  Additional processes / threads
                  <Input
                    type="number"
                    min={0}
                    max={limits.max_extra_processes}
                    value={extraProcesses}
                    disabled={busy}
                    onChange={(e) => setExtraProcesses(e.target.value)}
                  />
                  <span className="block text-xs font-normal text-ink/50">
                    Beyond the main guest process. Max {limits.max_extra_processes}
                  </span>
                </label>
              ) : null}
              <label className="space-y-1.5 text-sm font-medium text-ink sm:col-span-2">
                Disk MiB
                <Input
                  type="number"
                  min={1}
                  max={limits.max_disk_mib}
                  value={diskMib}
                  disabled={busy}
                  onChange={(e) => setDiskMib(e.target.value)}
                />
                <span className="block text-xs font-normal text-ink/50">
                  Max {limits.max_disk_mib} (each for data/ and tmp/)
                </span>
              </label>
              {!isWorkerd ? (
                <div className="sm:col-span-2">
                  <CpuCoresSlider
                    value={cpuCores}
                    onChange={setCpuCores}
                    hostMaxCores={limits.max_cpu_cores}
                    manifestCores={request.cpuCores ?? limits.cpu_cores}
                    globalCores={limits.jail_cpu_cores ?? null}
                    disabled={busy}
                  />
                </div>
              ) : null}
            </div>
          </ConsentSection>

          {isWorkerd ? (
            <ConsentSection
              id="consent-workerd"
              title="Isolate limits"
              summary={workerdSummary}
              icon={<Shield className="h-4 w-4" />}
              open={openSection === "workerd"}
              onToggle={() => toggleSection("workerd")}
            >
              <p className="text-xs text-ink/55">
                Workerd CPU soft budget and egress subrequest budget. Host maxes:{" "}
                {limits.max_cpu_ms.toLocaleString()} ms /{" "}
                {limits.max_subrequests.toLocaleString()} subrequests.
              </p>
              <div className="grid gap-3 sm:grid-cols-2">
                <label className="space-y-1.5 text-sm font-medium text-ink">
                  CPU milliseconds
                  <Input
                    type="number"
                    min={1}
                    max={limits.max_cpu_ms}
                    value={cpuMs}
                    disabled={busy}
                    onChange={(e) => setCpuMs(e.target.value)}
                  />
                </label>
                <label className="space-y-1.5 text-sm font-medium text-ink">
                  Subrequests
                  <Input
                    type="number"
                    min={1}
                    max={limits.max_subrequests}
                    value={subrequests}
                    disabled={busy}
                    onChange={(e) => setSubrequests(e.target.value)}
                  />
                </label>
              </div>
            </ConsentSection>
          ) : null}

          {isWorkerd ? (
            <ConsentSection
              id="consent-flags"
              title="Compatibility flags"
              summary={flagsSummary}
              icon={<Layers className="h-4 w-4" />}
              open={openSection === "flags"}
              onToggle={() => toggleSection("flags")}
            >
              <p className="text-xs text-ink/55">
                Workerd-only. Add or remove flags relative to the plugin request.
              </p>
              <ul className="space-y-2">
                {uniqueValues([
                  ...request.compatibilityFlags,
                  ...compatibilityFlags,
                ]).map((flag) => (
                  <li key={flag}>
                    <label className="flex cursor-pointer items-center gap-2.5 text-sm text-ink">
                      <input
                        type="checkbox"
                        className="h-4 w-4 accent-teal"
                        checked={compatibilityFlags.some(
                          (item) => item.toLowerCase() === flag.toLowerCase(),
                        )}
                        disabled={busy}
                        onChange={(e) =>
                          toggleValue(
                            flag,
                            e.target.checked,
                            setCompatibilityFlags,
                            compatibilityFlags,
                          )
                        }
                      />
                      {flag}
                    </label>
                  </li>
                ))}
              </ul>
              <div className="flex gap-2">
                <Input
                  value={flagDraft}
                  disabled={busy}
                  onChange={(e) => setFlagDraft(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      addFlag();
                    }
                  }}
                  placeholder="Add compatibility flag"
                  autoComplete="off"
                  spellCheck={false}
                />
                <Button
                  type="button"
                  variant="secondary"
                  disabled={busy || !flagDraft.trim()}
                  onClick={addFlag}
                >
                  Add
                </Button>
              </div>
            </ConsentSection>
          ) : null}

          <p className="border-t border-ink/10 py-4 text-xs leading-relaxed text-ink/55">
            Approving enables this plugin under the grant you configure. Removing
            capabilities the guest needs may break it. Bookclerk enforces the grant;
            it does not guarantee guest behaviour afterward.
            {consent.covered ? (
              <span className="mt-1 block text-teal">
                A covering grant is already stored; saving updates it.
              </span>
            ) : null}
          </p>
        </div>

        <div className="flex items-center justify-end gap-2 border-t border-ink/10 bg-paper/60 px-6 py-4">
          <Button type="button" variant="ghost" disabled={busy} onClick={onCancel}>
            Cancel
          </Button>
          <Button
            type="button"
            disabled={busy}
            className="min-w-[7.5rem]"
            onClick={() =>
              void onApprove({
                networkMode,
                domains: networkMode === "deny" ? [] : selectedDomains,
                bindings,
                compatibilityFlags: isWorkerd ? compatibilityFlags : [],
                cpuMs: isWorkerd
                  ? parsePositiveInt(cpuMs, limits.max_cpu_ms)
                  : undefined,
                subrequests: isWorkerd
                  ? parsePositiveInt(subrequests, limits.max_subrequests)
                  : undefined,
                diskMib: parsePositiveInt(diskMib, limits.max_disk_mib),
                memoryMib: parsePositiveInt(memoryMib, limits.max_memory_mib),
                cpuCores: isWorkerd ? undefined : cpuCores,
                extraProcesses: isWorkerd
                  ? undefined
                  : parseNonNegInt(extraProcesses, limits.max_extra_processes),
              })
            }
          >
            {busy ? "Approving..." : "Accept"}
          </Button>
        </div>
      </div>
    </div>
  );
}
