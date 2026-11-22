/** Stories for {@link Badge}: the status chips the library lists use. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Badge } from "./badge";

const meta = {
  title: "Primitives/Badge",
  component: Badge,
  args: { children: "Owned" },
} satisfies Meta<typeof Badge>;

export default meta;

/** A story of {@link Badge}. */
type Story = StoryObj<typeof meta>;

/** A neutral chip with no colour of its own. */
export const Plain: Story = {};

/** The tinted chips the app uses for status. */
export const Tones: Story = {
  render: () => (
    <div className="flex flex-wrap gap-2">
      <Badge className="bg-teal/15 text-ink">Owned</Badge>
      <Badge className="bg-brick/10 text-brick-text">Failed</Badge>
      <Badge className="bg-ink text-paper">New</Badge>
      <Badge className="bg-fold/60 text-ink">Wishlist</Badge>
    </div>
  ),
};
