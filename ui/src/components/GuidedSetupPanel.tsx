import { useEffect, useState, type KeyboardEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { initialDrafts, inputValue } from "@/lib/guidedSetup";
import {
  deprovisionPluginInstance,
  fetchPluginInstanceProvisioning,
  isApiError,
  provisionPluginInstance,
  type InstanceSettingValue,
  type PluginSettingsGroup,
  type ProvisioningView,
} from "@/lib/api";

/** Classes shared with the Settings page's dropdowns. */
const selectClassName =
  "w-full rounded-md border border-ink/15 bg-card-strong px-3 py-2 text-sm text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

interface GuidedSetupPanelProps {
  /** Instance group the panel sets up. */
  plugin: PluginSettingsGroup;
  /** Reloads the Settings snapshot after a run. */
  onChanged: () => Promise<void>;
  /** Called on a 401 so the page can return to sign-in. */
  onSessionExpired: () => void;
}

/**
 * The guided setup for an instance whose plugin declares `[provisioning]`:
 * the operator's choices, a link to create a token with the listed
 * permissions, the run, and what the plugin did. A plugin that removes what
 * it created also gets an undo button, to use before the instance is
 * deleted.
 *
 * Renders nothing when the plugin offers no guided setup on this server.
 */
export function GuidedSetupPanel({ plugin, onChanged, onSessionExpired }: GuidedSetupPanelProps) {
  const instanceId = plugin.plugin_instance_id;
  const [table, setTable] = useState<ProvisioningView | null>(null);
  const [inputs, setInputs] = useState<Record<string, string>>({});
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [summary, setSummary] = useState<string[]>([]);

  useEffect(() => {
    if (!instanceId) return;
    let live = true;
    fetchPluginInstanceProvisioning(instanceId)
      .then((found) => {
        if (!live) return;
        setTable(found);
        if (found) setInputs(initialDrafts(found, plugin.settings));
      })
      .catch((err: unknown) => {
        if (!live) return;
        if (isApiError(err) && err.status === 401) onSessionExpired();
        // 409: the plugin is not installed on this server, so its setup
        // cannot run here; the panel stays hidden.
        setTable(null);
      });
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- drafts start from the settings as the panel opened; reloads keep what the operator typed
  }, [instanceId, onSessionExpired]);

  if (!instanceId || !table) return null;

  async function act(action: () => Promise<string[]>) {
    setBusy(true);
    setError(null);
    try {
      setSummary(await action());
      setToken("");
      await onChanged();
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(err instanceof Error ? err.message : "The guided setup failed");
    } finally {
      setBusy(false);
    }
  }

  function run() {
    if (!token.trim()) {
      setError("Paste an API token first.");
      return;
    }
    const chosen: Record<string, InstanceSettingValue> = {};
    for (const [key, draft] of Object.entries(inputs)) {
      const value = inputValue(
        plugin.settings.find((option) => option.key === key),
        draft,
      );
      if (value !== null) chosen[key] = value;
    }
    void act(async () => {
      const done = await provisionPluginInstance(instanceId as string, {
        expected_revision: plugin.config_revision ?? 0,
        token: token.trim(),
        inputs: chosen,
      });
      return done.summary.length ? done.summary : ["Setup finished."];
    });
  }

  function undo() {
    void act(async () => {
      await deprovisionPluginInstance(instanceId as string, token.trim() || undefined);
      return ["Removed what the guided setup created."];
    });
  }

  /** Enter runs the setup instead of submitting the Settings form. */
  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "Enter") {
      event.preventDefault();
      run();
    }
  }

  const tokenId = `guided-setup-token-${instanceId}`;
  return (
    <div className="mt-3 border-t border-ink/10 pt-3">
      <p className="text-xs font-medium text-ink/70">Guided setup</p>
      <p className="mt-1 text-xs text-ink/70">
        Paste an API token and this plugin creates what it needs at {table.provider} and fills in
        its settings. The token is used for this run only and is not stored.
      </p>
      {table.token_permissions.length ? (
        <div className="mt-2 text-xs text-ink/70">
          <p>The token needs these permissions:</p>
          <ul className="mt-1 list-disc pl-5">
            {table.token_permissions.map((line) => (
              <li key={line}>{line}</li>
            ))}
          </ul>
        </div>
      ) : null}
      {table.token_url ? (
        <p className="mt-2 text-xs">
          <a
            className="text-teal underline"
            href={table.token_url}
            target="_blank"
            rel="noopener noreferrer"
          >
            Create a token at {table.provider}
          </a>
        </p>
      ) : null}
      <div className="mt-2 grid gap-2 sm:grid-cols-2">
        {table.inputs.map((key) => {
          const option = plugin.settings.find((candidate) => candidate.key === key);
          const fieldId = `guided-setup-${instanceId}-${key}`;
          return (
            <label key={key} htmlFor={fieldId} className="grid gap-1 text-xs text-ink/70">
              {option?.label || key}
              {option?.choices?.length ? (
                <select
                  id={fieldId}
                  className={selectClassName}
                  value={inputs[key] ?? ""}
                  disabled={busy}
                  onChange={(e) => setInputs({ ...inputs, [key]: e.target.value })}
                >
                  {option.choices.map((choice) => (
                    <option key={choice.value} value={choice.value}>
                      {choice.label}
                    </option>
                  ))}
                </select>
              ) : (
                <Input
                  id={fieldId}
                  value={inputs[key] ?? ""}
                  disabled={busy}
                  onChange={(e) => setInputs({ ...inputs, [key]: e.target.value })}
                  onKeyDown={onKeyDown}
                />
              )}
            </label>
          );
        })}
        <label htmlFor={tokenId} className="grid gap-1 text-xs text-ink/70">
          API token
          <Input
            id={tokenId}
            type="password"
            autoComplete="off"
            value={token}
            disabled={busy}
            onChange={(e) => setToken(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>
      </div>
      <div className="mt-2 flex flex-wrap gap-2">
        <Button type="button" variant="secondary" disabled={busy} onClick={run}>
          Run guided setup
        </Button>
        {table.deprovision ? (
          <Button
            type="button"
            variant="ghost"
            disabled={busy}
            title="Removes what the guided setup created, using the stored credentials or the token above"
            onClick={undo}
          >
            Remove what setup created
          </Button>
        ) : null}
      </div>
      {error ? (
        <p role="alert" className="mt-2 text-xs text-brick-text">
          {error}
        </p>
      ) : null}
      {summary.length ? (
        <ul aria-label="Guided setup result" className="mt-2 list-disc pl-5 text-xs text-ink/70">
          {summary.map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
