/** Stories for {@link Input}: empty, filled and invalid, each with a label. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Input } from "./input";

const meta = {
  title: "Primitives/Input",
  component: Input,
  // An input needs an accessible name; the app always pairs one with a label.
  render: (args) => (
    <label className="block max-w-sm space-y-1 text-sm font-medium text-ink">
      <span>Server address</span>
      <Input {...args} />
    </label>
  ),
} satisfies Meta<typeof Input>;

export default meta;

/** A story of {@link Input}. */
type Story = StoryObj<typeof meta>;

/** Empty, showing the placeholder. */
export const Empty: Story = { args: { placeholder: "library.example.com" } };

/** Holding a value. */
export const Filled: Story = { args: { defaultValue: "library.example.com" } };

/** Marked invalid, with the brick border. */
export const Invalid: Story = {
  args: { defaultValue: "not a host name", "aria-invalid": true },
};
