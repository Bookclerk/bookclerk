import { useId } from "react";
import { cn } from "@/lib/utils";

export type CpuCoresSliderProps = {
  /** Current value in cores (0.01 resolution). */
  value: number;
  /** Called with cores when the operator moves the slider or types. */
  onChange: (cores: number) => void;
  /** Host max in cores (from API). */
  hostMaxCores: number;
  /** Manifest / baseline cores (marker). */
  manifestCores?: number | null;
  /** Global Settings ceiling in cores (marker). */
  globalCores?: number | null;
  /** Minimum selectable cores (default 0.01). */
  minCores?: number;
  disabled?: boolean;
  id?: string;
  className?: string;
};

function formatCores(cores: number): string {
  return (Number.isFinite(cores) ? cores : 0).toFixed(2);
}

function clamp(cores: number, min: number, max: number): number {
  const stepped = Math.round(cores * 100) / 100;
  return Math.min(max, Math.max(min, stepped));
}

/**
 * Jail CPU control as cores (two decimal places) on a 0→host-max slider.
 *
 * Markers show the plugin manifest baseline and optional global Settings cap.
 * Values are already cores from the daemon — no unit conversion here.
 */
export function CpuCoresSlider({
  value,
  onChange,
  hostMaxCores,
  manifestCores,
  globalCores,
  minCores = 0.01,
  disabled,
  id,
  className,
}: CpuCoresSliderProps) {
  const autoId = useId();
  const controlId = id ?? autoId;
  const hostMax = Math.max(minCores, hostMaxCores);
  const selectableMax =
    globalCores != null && globalCores > 0
      ? Math.min(hostMax, globalCores)
      : hostMax;
  const cores = clamp(value, minCores, selectableMax);

  function markLeft(coresAt: number): string {
    const pct = (coresAt / hostMax) * 100;
    return `${Math.min(100, Math.max(0, pct))}%`;
  }

  return (
    <div className={cn("flex flex-col gap-2", className)}>
      <div className="flex items-baseline justify-between gap-3">
        <label htmlFor={controlId} className="text-sm font-medium text-ink">
          CPU cores
        </label>
        <div className="flex items-center gap-2">
          <input
            type="number"
            inputMode="decimal"
            step={0.01}
            min={minCores}
            max={selectableMax}
            disabled={disabled}
            value={formatCores(cores)}
            aria-label="CPU cores"
            className="w-24 rounded-md border border-ink/15 bg-card-strong px-2 py-1 text-right text-sm tabular-nums text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
            onChange={(e) => {
              const next = Number.parseFloat(e.target.value);
              if (!Number.isFinite(next)) return;
              onChange(clamp(next, minCores, selectableMax));
            }}
          />
          <span className="text-xs text-ink/50">cores</span>
        </div>
      </div>

      <div className="relative pt-1 pb-5">
        <input
          id={controlId}
          type="range"
          min={minCores}
          max={hostMax}
          step={0.01}
          disabled={disabled}
          value={cores}
          aria-valuemin={minCores}
          aria-valuemax={selectableMax}
          aria-valuenow={cores}
          aria-valuetext={`${formatCores(cores)} cores`}
          className={cn(
            "h-2 w-full cursor-pointer appearance-none rounded-full bg-ink/10 accent-teal",
            "disabled:cursor-not-allowed disabled:opacity-50",
            "[&::-webkit-slider-thumb]:size-4 [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-full [&::-webkit-slider-thumb]:bg-teal",
            "[&::-moz-range-thumb]:size-4 [&::-moz-range-thumb]:rounded-full [&::-moz-range-thumb]:border-0 [&::-moz-range-thumb]:bg-teal",
          )}
          onChange={(e) => {
            const raw = Number.parseFloat(e.target.value);
            onChange(clamp(raw, minCores, selectableMax));
          }}
        />

        <div className="pointer-events-none absolute inset-x-0 top-1 h-2">
          {manifestCores != null ? (
            <span
              title={`Manifest ${formatCores(manifestCores)}`}
              className="absolute top-0 h-2 w-0.5 -translate-x-1/2 bg-ink/45"
              style={{ left: markLeft(manifestCores) }}
            />
          ) : null}
          {globalCores != null ? (
            <span
              title={`Global cap ${formatCores(globalCores)}`}
              className="absolute top-0 h-2 w-0.5 -translate-x-1/2 bg-teal"
              style={{ left: markLeft(globalCores) }}
            />
          ) : null}
        </div>

        <div className="absolute inset-x-0 top-5 flex justify-between text-[11px] leading-tight text-ink/50">
          <span>0.00</span>
          <span className="text-center">
            {manifestCores != null ? (
              <span className="block">Manifest {formatCores(manifestCores)}</span>
            ) : null}
            {globalCores != null ? (
              <span className="block text-teal">Global {formatCores(globalCores)}</span>
            ) : (
              <span className="block">No global cap</span>
            )}
          </span>
          <span>Host {formatCores(hostMax)}</span>
        </div>
      </div>
    </div>
  );
}
