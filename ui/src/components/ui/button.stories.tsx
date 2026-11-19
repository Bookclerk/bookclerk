/** Stories for {@link Button}: every variant, enabled and disabled. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Button } from "./button";

const meta = {
  title: "Primitives/Button",
  component: Button,
  args: { children: "Acquire" },
} satisfies Meta<typeof Button>;

export default meta;

/** A story of {@link Button}. */
type Story = StoryObj<typeof meta>;

/** The default call to action. */
export const Primary: Story = { args: { variant: "primary" } };

/** A secondary action beside a primary one. */
export const Secondary: Story = { args: { variant: "secondary", children: "Open details" } };

/** A low-emphasis action, such as Cancel. */
export const Ghost: Story = { args: { variant: "ghost", children: "Cancel" } };

/** A destructive action. */
export const Danger: Story = { args: { variant: "danger", children: "Revoke account" } };

/** Every variant side by side, disabled, to check the dimmed contrast. */
export const Disabled: Story = {
  render: () => (
    <div className="flex flex-wrap gap-3">
      <Button disabled>Primary</Button>
      <Button variant="secondary" disabled>
        Secondary
      </Button>
      <Button variant="ghost" disabled>
        Ghost
      </Button>
      <Button variant="danger" disabled>
        Danger
      </Button>
    </div>
  ),
};
