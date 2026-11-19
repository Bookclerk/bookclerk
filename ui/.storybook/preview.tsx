/**
 * Global story setup: the SPA's stylesheet and a light / dark theme switch.
 *
 * The theme decorator toggles `html.dark` exactly as `ThemeProvider` does, so
 * a story renders with the same tokens as the app. The visual checks select
 * the theme with the `globals=theme:dark` query parameter.
 */
import type { Decorator, Preview } from "@storybook/react-vite";
import "../src/index.css";
import "./preview.css";

/**
 * Applies the story's `theme` global to the document root before the story
 * renders, so its first paint already has the right tokens.
 */
const withTheme: Decorator = (Story, context) => {
  document.documentElement.classList.toggle("dark", context.globals.theme === "dark");
  return <Story />;
};

const preview: Preview = {
  decorators: [withTheme],
  globalTypes: {
    theme: {
      description: "Light or dark appearance",
      toolbar: {
        title: "Theme",
        icon: "mirror",
        items: [
          { value: "light", title: "Light" },
          { value: "dark", title: "Dark" },
        ],
        dynamicTitle: true,
      },
    },
  },
  initialGlobals: { theme: "light" },
  parameters: {
    layout: "padded",
  },
};

export default preview;
