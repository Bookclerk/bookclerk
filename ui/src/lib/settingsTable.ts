/**
 * Helpers for `table` plugin settings: rows of records whose fields are the
 * columns the plugin declares.
 *
 * The Settings form keeps every value as a string, so a table's value is the
 * JSON text of its rows. These helpers move between that text, the typed rows
 * the instance document stores, and the strings each cell's input shows.
 *
 * @module
 */
import type { PluginSettingColumn, SettingCellValue, SettingRow } from "./api";

/**
 * Reads the rows out of a table setting's JSON text.
 *
 * @param text - JSON text of the rows, as the Settings response sends it.
 * @returns The rows; an empty list when the text is empty or not an array of
 *   objects, and only the scalar cells of each row.
 */
export function parseRows(text: string): SettingRow[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  return parsed
    .filter((row): row is Record<string, unknown> => !!row && typeof row === "object")
    .filter((row) => !Array.isArray(row))
    .map((row) => {
      const out: SettingRow = {};
      for (const [key, cell] of Object.entries(row)) {
        if (typeof cell === "string" || typeof cell === "boolean") out[key] = cell;
        else if (typeof cell === "number" && Number.isFinite(cell)) out[key] = cell;
      }
      return out;
    });
}

/**
 * Writes rows back to the JSON text the Settings form holds.
 *
 * @param rows - Rows to store.
 * @returns Compact JSON text of the rows.
 */
export function serializeRows(rows: SettingRow[]): string {
  return JSON.stringify(rows);
}

/**
 * Text a cell's input shows.
 *
 * @param value - Stored cell, or `undefined` when the row leaves it out.
 * @returns The cell as text; empty when absent.
 */
export function cellText(value: SettingCellValue | undefined): string {
  return value === undefined ? "" : String(value);
}

/**
 * Typed cell for the text a column's input holds.
 *
 * @param column - Column the cell belongs to.
 * @param text - Text from the input.
 * @returns A boolean for a boolean column, a number for a number column, the
 *   text otherwise; `undefined` when the text is not a finite number, or is
 *   empty and the column has no non-empty default, so the row leaves the
 *   column out. An empty string column with a non-empty default keeps `""`,
 *   since leaving it out would bring the default back.
 */
export function cellValue(column: PluginSettingColumn, text: string): SettingCellValue | undefined {
  if (column.value_type === "boolean") {
    const normalized = text.trim().toLowerCase();
    return (
      normalized === "true" || normalized === "1" || normalized === "yes" || normalized === "on"
    );
  }
  if (!text.trim()) {
    const overridesDefault =
      column.value_type === "string" && !column.choices?.length && !!column.default;
    return overridesDefault ? "" : undefined;
  }
  if (column.value_type === "number") {
    const number = Number(text);
    return Number.isFinite(number) ? number : undefined;
  }
  return text;
}

/**
 * Returns `row` with one cell changed.
 *
 * @param row - Row to change; not modified.
 * @param column - Column of the cell.
 * @param text - New text from the cell's input.
 * @returns A new row; the column is left out when the text holds no value.
 */
export function withCell(row: SettingRow, column: PluginSettingColumn, text: string): SettingRow {
  const next = { ...row };
  const value = cellValue(column, text);
  if (value === undefined) delete next[column.key];
  else next[column.key] = value;
  return next;
}

/**
 * A new row filled with each column's default.
 *
 * @param columns - Columns of the table.
 * @returns A row holding the columns that declare a default, and `false`
 *   for each boolean column without one, so the row holds the value its
 *   unchecked box shows.
 */
export function newRow(columns: PluginSettingColumn[]): SettingRow {
  let row: SettingRow = {};
  for (const column of columns) {
    if (column.default !== undefined) row = withCell(row, column, column.default);
    else if (column.value_type === "boolean") row = withCell(row, column, "false");
  }
  return row;
}

/**
 * The first problem that would make the server refuse the rows.
 *
 * @param columns - Columns of the table.
 * @param rows - Rows to check.
 * @returns A message naming the row (counted from 1) and the column, or
 *   `null` when every required column has a value and every choice is valid.
 */
export function rowsProblem(columns: PluginSettingColumn[], rows: SettingRow[]): string | null {
  for (const [index, row] of rows.entries()) {
    for (const column of columns) {
      const value = row[column.key];
      if (value === undefined) {
        if (column.required && column.default === undefined) {
          return `Row ${index + 1}: ${column.label} is required`;
        }
        continue;
      }
      if (column.choices?.length && !column.choices.some((choice) => choice.value === value)) {
        return `Row ${index + 1}: choose a valid ${column.label}`;
      }
    }
  }
  return null;
}
