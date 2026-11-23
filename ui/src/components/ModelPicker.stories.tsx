/** Stories for {@link ModelPicker}: installed, downloading, failed and a model that does not fit. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import type { PluginModelsView } from "@/lib/api";
import { ModelPicker } from "./ModelPicker";

/** The two embedding models, the default installed. */
const models: PluginModelsView = {
  setting: "model",
  chosen: "bekko-a8m",
  memory_ceiling_bytes: 512 * 2 ** 20,
  models: [
    {
      id: "bekko-a8m",
      label: "Bekko (fast)",
      summary: "Small and quick. The default, and right for most libraries.",
      download_bytes: 140_609_206,
      memory_bytes: 230_686_720,
      fits: true,
      state: "installed",
    },
    {
      id: "harrier-270m-q4",
      label: "Harrier (most accurate)",
      summary: "Better matches, most of all outside English. Slower, and needs more memory.",
      download_bytes: 217_276_472,
      memory_bytes: 398_458_880,
      fits: true,
      state: "not_installed",
    },
  ],
};

/** The same models with the first one in another state. */
function withFirst(over: Partial<PluginModelsView["models"][number]>): PluginModelsView {
  const [first, ...rest] = models.models;
  return { ...models, models: first ? [{ ...first, ...over }, ...rest] : rest };
}

const meta = {
  title: "Settings/ModelPicker",
  component: ModelPicker,
  args: {
    id: "integration-embedder-model",
    label: "Model",
    models,
    value: "bekko-a8m",
    onChange: () => {},
  },
} satisfies Meta<typeof ModelPicker>;

export default meta;

/** A story of {@link ModelPicker}. */
type Story = StoryObj<typeof meta>;

/** The default model, installed. */
export const Installed: Story = {};

/** The first download, part way. */
export const Downloading: Story = {
  args: { models: withFirst({ state: "downloading", received_bytes: 52_428_800 }) },
};

/** A download that failed and is tried again. */
export const Failed: Story = {
  args: { models: withFirst({ state: "waiting", detail: "the registry answered 503" }) },
};

/** The other model selected and not saved yet. */
export const Switching: Story = { args: { value: "harrier-270m-q4" } };

/** A server whose plugin memory is too small for the larger model. */
export const DoesNotFit: Story = {
  args: {
    models: {
      ...models,
      memory_ceiling_bytes: 256 * 2 ** 20,
      models: models.models.map((model) =>
        model.id === "harrier-270m-q4" ? { ...model, fits: false } : model,
      ),
    },
  },
};
