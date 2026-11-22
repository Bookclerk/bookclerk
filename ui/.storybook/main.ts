/**
 * Storybook configuration for the operator SPA's component catalog.
 *
 * Storybook is development and CI tooling only: `vite build` never imports a
 * story, so nothing here ships in `ui/dist`. The visual and accessibility
 * checks (`visual/`) run against the static build this produces
 * (`npm run storybook:build`). See docs/contributing/ui/visual-testing.md.
 */
import type { StorybookConfig } from "@storybook/react-vite";

const config: StorybookConfig = {
  stories: ["../src/**/*.stories.tsx"],
  framework: { name: "@storybook/react-vite", options: {} },
  // Not minified, so components keep their names and the visual suite can
  // tell which ones each story rendered (visual/coverage.ts).
  viteFinal: (vite) => ({ ...vite, build: { ...vite.build, minify: false } }),
  core: {
    // No outside service: no usage reports, no update checks.
    disableTelemetry: true,
    disableWhatsNewNotifications: true,
  },
};

export default config;
