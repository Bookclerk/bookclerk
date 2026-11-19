/** Stories for {@link NotFoundPage}, the SPA's unknown-path screen. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { NotFoundPage } from "./NotFoundPage";

const meta = {
  title: "Pages/NotFoundPage",
  component: NotFoundPage,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof NotFoundPage>;

export default meta;

/** A story of {@link NotFoundPage}. */
type Story = StoryObj<typeof meta>;

/** The page as a visitor to an unknown path sees it. */
export const Default: Story = {};
