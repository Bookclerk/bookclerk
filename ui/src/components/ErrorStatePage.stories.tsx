/** Stories for {@link ErrorStatePage}: with and without a retry action. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { ErrorStatePage } from "./ErrorStatePage";

const meta = {
  title: "Pages/ErrorStatePage",
  component: ErrorStatePage,
  args: {
    title: "Could not load the library",
    message: "The daemon did not answer. Check that it is running, then try again.",
  },
} satisfies Meta<typeof ErrorStatePage>;

export default meta;

/** A story of {@link ErrorStatePage}. */
type Story = StoryObj<typeof meta>;

/** A recoverable error with a retry button. */
export const WithRetry: Story = { args: { onRetry: () => {} } };

/** An error the page cannot retry. */
export const MessageOnly: Story = {};
