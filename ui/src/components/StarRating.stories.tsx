/** Stories for {@link StarRating}: whole, fractional and out-of-range values. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { StarRating } from "./StarRating";

const meta = {
  title: "Library/StarRating",
  component: StarRating,
  args: { value: 4 },
} satisfies Meta<typeof StarRating>;

export default meta;

/** A story of {@link StarRating}. */
type Story = StoryObj<typeof meta>;

/** A whole-star rating. */
export const Whole: Story = {};

/** A fractional rating, with the last star clipped. */
export const Fractional: Story = { args: { value: 3.6 } };

/** No rating yet: every star empty. */
export const Unrated: Story = { args: { value: 0 } };

/** The larger glyphs the detail dialog uses. */
export const Large: Story = { args: { value: 4.5, starClassName: "h-6 w-6" } };
