import { describe, expect, it } from "vitest";
import type { PluginSettingColumn } from "./api";
import {
  cellText,
  cellValue,
  newRow,
  parseRows,
  rowsProblem,
  serializeRows,
  withCell,
} from "./settingsTable";

const library: PluginSettingColumn = {
  key: "library",
  label: "Library",
  value_type: "string",
  required: true,
};
const depth: PluginSettingColumn = { key: "depth", label: "Depth", value_type: "number" };
const watch: PluginSettingColumn = {
  key: "watch",
  label: "Watch",
  value_type: "boolean",
  default: "true",
};
const mode: PluginSettingColumn = {
  key: "mode",
  label: "Mode",
  value_type: "string",
  choices: [
    { value: "read", label: "Read" },
    { value: "write", label: "Write" },
  ],
};
const columns = [library, depth, watch, mode];

describe("table settings", () => {
  it("reads only arrays of objects and their scalar cells", () => {
    expect(parseRows('[{"library":"a","depth":2,"watch":false,"x":[1],"y":null}]')).toEqual([
      { library: "a", depth: 2, watch: false },
    ]);
    expect(parseRows("")).toEqual([]);
    expect(parseRows("{}")).toEqual([]);
    expect(parseRows('["a", [1], null, {"library":"b"}]')).toEqual([{ library: "b" }]);
    expect(serializeRows([{ library: "a" }])).toBe('[{"library":"a"}]');
  });

  it("types cells by column", () => {
    expect(cellValue(watch, "on")).toBe(true);
    expect(cellValue(watch, "false")).toBe(false);
    expect(cellValue(depth, "3")).toBe(3);
    expect(cellValue(depth, "x")).toBeUndefined();
    expect(cellValue(library, " ")).toBeUndefined();
    expect(cellValue(library, "main")).toBe("main");
    expect(cellText(undefined)).toBe("");
    expect(cellText(false)).toBe("false");
  });

  it("edits a copy of the row and drops empty cells", () => {
    const row = { library: "a", depth: 2 };
    expect(withCell(row, depth, "")).toEqual({ library: "a" });
    expect(withCell(row, library, "b")).toEqual({ library: "b", depth: 2 });
    expect(row).toEqual({ library: "a", depth: 2 });
  });

  it("starts new rows from column defaults", () => {
    expect(newRow(columns)).toEqual({ watch: true });
    const strict: PluginSettingColumn = {
      key: "strict",
      label: "Strict",
      value_type: "boolean",
      required: true,
    };
    expect(newRow([library, strict])).toEqual({ strict: false });
    expect(rowsProblem([library, strict], [{ ...newRow([library, strict]), library: "a" }])).toBe(
      null,
    );
  });

  it("keeps an empty string that overrides a default", () => {
    const prefix: PluginSettingColumn = {
      key: "prefix",
      label: "Prefix",
      value_type: "string",
      default: "books/",
    };
    expect(cellValue(prefix, "")).toBe("");
    expect(withCell({ library: "a", prefix: "x" }, prefix, "")).toEqual({
      library: "a",
      prefix: "",
    });
    expect(withCell({ library: "a", prefix: "x" }, library, "")).toEqual({ prefix: "x" });
  });

  it("reports missing required cells and invalid choices", () => {
    expect(rowsProblem(columns, [{ library: "a", mode: "read" }])).toBeNull();
    expect(rowsProblem(columns, [{ library: "a" }, { depth: 1 }])).toBe(
      "Row 2: Library is required",
    );
    expect(rowsProblem(columns, [{ library: "a", mode: "both" }])).toBe(
      "Row 1: choose a valid Mode",
    );
  });
});
