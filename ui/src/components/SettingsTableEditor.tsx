import { Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { PluginSettingColumn } from "@/lib/api";
import { cellText, newRow, parseRows, serializeRows, withCell } from "@/lib/settingsTable";

/** Class names shared with the Settings page's other dropdowns. */
const cellSelectClassName =
  "w-full rounded-md border border-ink/15 bg-card-strong px-2 py-1.5 text-sm text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

/**
 * Editor for a `table` plugin setting: one row per record, one input per
 * declared column, with buttons to add and remove rows.
 *
 * @param props - The field id, caption, columns, the rows as JSON text, an
 *   optional error, and the change callback that receives new JSON text.
 */
export function SettingsTableEditor({
  id,
  label,
  columns,
  value,
  error,
  onChange,
}: {
  id: string;
  label: string;
  columns: PluginSettingColumn[];
  value: string;
  error?: string;
  onChange: (next: string) => void;
}) {
  const rows = parseRows(value);
  const update = (next: typeof rows) => onChange(serializeRows(next));

  return (
    <fieldset
      className="space-y-2 sm:col-span-2"
      aria-describedby={error ? `${id}-error` : undefined}
    >
      <legend className="text-sm font-medium text-ink">{label}</legend>
      {rows.length === 0 ? (
        <p className="text-xs text-ink/50">No rows yet.</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-sm">
            <thead>
              <tr>
                {columns.map((column) => (
                  <th
                    key={column.key}
                    scope="col"
                    className="px-1 pb-1 text-left text-xs font-medium text-ink/60"
                  >
                    {column.label}
                    {column.required ? <span aria-hidden="true"> *</span> : null}
                  </th>
                ))}
                <th scope="col" className="sr-only">
                  Remove
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row, index) => (
                <tr key={index}>
                  {columns.map((column) => {
                    const cellId = `${id}-${index}-${column.key}`;
                    const text = cellText(row[column.key]);
                    const setText = (next: string) =>
                      update(rows.map((r, i) => (i === index ? withCell(r, column, next) : r)));
                    const name = `${column.label}, row ${index + 1}`;
                    return (
                      <td key={column.key} className="px-1 py-0.5 align-middle">
                        {column.value_type === "boolean" ? (
                          <input
                            id={cellId}
                            type="checkbox"
                            aria-label={name}
                            checked={row[column.key] === true}
                            onChange={(e) => setText(String(e.target.checked))}
                            className="h-4 w-4 accent-teal"
                          />
                        ) : column.choices?.length ? (
                          <select
                            id={cellId}
                            aria-label={name}
                            value={text}
                            onChange={(e) => setText(e.target.value)}
                            className={cellSelectClassName}
                          >
                            {column.required ? null : <option value="">—</option>}
                            {column.choices.map((choice) => (
                              <option key={choice.value} value={choice.value}>
                                {choice.label}
                              </option>
                            ))}
                          </select>
                        ) : (
                          <Input
                            id={cellId}
                            aria-label={name}
                            type={column.value_type === "number" ? "number" : "text"}
                            value={text}
                            onChange={(e) => setText(e.target.value)}
                          />
                        )}
                      </td>
                    );
                  })}
                  <td className="px-1 py-0.5 text-right align-middle">
                    <Button
                      type="button"
                      variant="ghost"
                      aria-label={`Remove row ${index + 1}`}
                      onClick={() => update(rows.filter((_, i) => i !== index))}
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <Button type="button" variant="secondary" onClick={() => update([...rows, newRow(columns)])}>
        <Plus className="h-4 w-4" />
        Add row
      </Button>
      {error ? (
        <p id={`${id}-error`} className="text-xs text-brick" role="alert">
          {error}
        </p>
      ) : null}
    </fieldset>
  );
}
