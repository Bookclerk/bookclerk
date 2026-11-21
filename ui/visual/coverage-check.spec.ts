/**
 * Checks the coverage check's reading of a module's exports, so a component
 * exported in a way it does not recognize cannot go unchecked.
 */
import { expect, test } from "@playwright/test";
import { exportedNames } from "./coverage-check.ts";

test("coverage check reads inline exports and export lists", () => {
  // Pure parsing: once is enough.
  test.skip(test.info().project.name !== "desktop", "runs once, on desktop");
  const source = [
    "export function Covered() { return null; }",
    "function Missing() { return null; }",
    "const Renamed = () => null;",
    "type Props = { a: string };",
    "export const LIMIT = 3;",
    "export {",
    "  Missing,",
    "  Renamed as Shown,",
    "  type Props,",
    "};",
    'export { Elsewhere } from "./elsewhere";',
  ].join("\n");
  expect(exportedNames(source).sort()).toEqual(["Covered", "Missing", "Renamed"]);
  expect(exportedNames("function Page() { return null; }\nexport default Page;")).toEqual(["Page"]);
});
