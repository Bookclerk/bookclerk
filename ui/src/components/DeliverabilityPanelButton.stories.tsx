/** Stories for {@link DeliverabilityPanel}: the notifier settings section before a check runs. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import type { PluginSettingsGroup } from "@/lib/api";
import { DeliverabilityPanel } from "./DeliverabilityPanel";

/** An email notifier instance whose plugin offers a deliverability check. */
const emailNotifier: PluginSettingsGroup = {
  id: "notifier-email",
  family: "notifier",
  plugin_key: "notifier-email",
  plugin_instance_id: "00000000-0000-4000-8000-000000000001",
  display_name: "Email",
  settings: [],
  deliverability_check: true,
};

const meta = {
  title: "Settings/DeliverabilityPanel",
  component: DeliverabilityPanel,
  args: { plugin: emailNotifier, onSessionExpired: () => {} },
} satisfies Meta<typeof DeliverabilityPanel>;

export default meta;

/** A story of {@link DeliverabilityPanel}. */
type Story = StoryObj<typeof meta>;

/** The explanation and the button, before the operator runs a check. */
export const Ready: Story = {};
