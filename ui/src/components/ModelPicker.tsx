import type { PluginModelsView } from "@/lib/api";
import { modelCost, modelStatus, modelUnfit } from "@/lib/modelPicker";
import { cn } from "@/lib/utils";

/**
 * Settings → a model plugin's "Model" setting: one row per offered model
 * with what it costs (download size, memory) and where its files stand on
 * this server. A model that needs more memory than the plugin may use here
 * cannot be chosen, unless it already is the choice.
 */
export function ModelPicker({
  id,
  label,
  models,
  value,
  error,
  onChange,
}: {
  /** Prefix for the inputs' ids and the group's name. */
  id: string;
  /** The setting's label. */
  label: string;
  /** The offered models, as this server has them. */
  models: PluginModelsView;
  /** The model id the form has selected. */
  value: string;
  /** Validation error for the setting, when any. */
  error?: string;
  /** Called with the model id the operator picks. */
  onChange: (modelId: string) => void;
}) {
  return (
    <fieldset className="space-y-2">
      <legend className="text-sm font-medium text-ink">{label}</legend>
      {models.models.map((model) => {
        const selected = model.id === value;
        const unfit = modelUnfit(model, models.memory_ceiling_bytes);
        const status = modelStatus(model, selected);
        const inputId = `${id}-${model.id}`;
        return (
          <label
            key={model.id}
            htmlFor={inputId}
            className={cn(
              "flex items-start gap-3 rounded-md border px-3 py-2 text-sm",
              selected ? "border-teal bg-teal/5" : "border-ink/15 bg-card-strong",
              unfit && !selected ? "border-dashed" : "cursor-pointer",
            )}
          >
            <input
              id={inputId}
              type="radio"
              name={id}
              className="mt-1"
              value={model.id}
              checked={selected}
              disabled={unfit !== null && !selected}
              onChange={() => onChange(model.id)}
            />
            <span className="min-w-0 flex-1 space-y-0.5">
              <span className="block font-medium text-ink">{model.label}</span>
              <span className="block text-ink/70">{model.summary}</span>
              <span className="block text-xs text-ink/70">{modelCost(model)}</span>
              {unfit ? (
                <span className="block text-xs font-medium text-brick-text">{unfit}</span>
              ) : null}
              {status ? (
                <span
                  className={cn(
                    "block text-xs",
                    status.trouble ? "font-medium text-brick-text" : "text-ink/70",
                  )}
                >
                  {status.label}
                </span>
              ) : null}
            </span>
          </label>
        );
      })}
      {error ? (
        <p className="text-xs font-medium text-brick-text" role="alert">
          {error}
        </p>
      ) : null}
    </fieldset>
  );
}
