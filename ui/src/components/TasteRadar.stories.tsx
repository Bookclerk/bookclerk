/** Stories for {@link TasteRadarChart}: six, four and three axes. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { TasteRadarChart } from "./TasteRadar";

/** An axis named `name` with `score`, made by `books` books and wishes. */
function axis(name: string, score: number, books: number, examples: string[]) {
  return {
    name,
    score,
    books,
    examples: examples.map((title, i) => ({ work_id: `${name}-${i}`, title })),
  };
}

const meta = {
  title: "Discover/TasteRadar",
  component: TasteRadarChart,
  args: {
    axes: [
      axis("Fantasy", 0.86, 14, ["The Name of the Wind", "Mistborn", "A Wizard of Earthsea"]),
      axis("Science Fiction", 0.58, 9, ["Dune", "Project Hail Mary", "The Left Hand of Darkness"]),
      axis("Discworld", 0.31, 4, ["Good Omens", "Guards! Guards!"]),
      axis("Mystery", 0.12, 6, ["The Thursday Murder Club", "Gaudy Night", "The Big Sleep"]),
      axis("Like SPQR", 0.05, 5, ["SPQR", "The Guns of August", "1491"]),
      axis("New York Times bestseller", 0.22, 3, ["Born a Crime", "Educated"]),
    ],
  },
  decorators: [
    (Story) => (
      <div className="max-w-xl">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof TasteRadarChart>;

export default meta;

/** A story of {@link TasteRadarChart}. */
type Story = StoryObj<typeof meta>;

/** Six axes, the most a radar has; the tile shows the closest one. */
export const SixAxes: Story = {};

/** Four axes. */
export const FourAxes: Story = {
  args: { axes: meta.args.axes.slice(0, 4) },
};

/** Three axes, the fewest that make a radar, with a book far from all. */
export const ThreeAxesFarFromAll: Story = {
  args: {
    axes: [
      axis("Cooking", 0.08, 3, ["Salt, Fat, Acid, Heat", "Bread at Home"]),
      axis("Travel", 0.0, 2, ["In Patagonia"]),
      axis("Poetry", 0.15, 2, []),
    ],
  },
};
