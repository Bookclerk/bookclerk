# Visual and accessibility testing

CI checks the web app's components for layout changes and accessibility
problems. Each component has Storybook stories beside its source. CI opens
every story in a browser, checks it with axe-core, and compares a screenshot
with an approved baseline image.

Nothing here ships in the app. Storybook, Playwright and the stories are
development tools, and `vite build` never imports a story, so `ui/dist` is
unchanged.

## What runs

For every story in the static Storybook build, the check:

1. Opens the story at two viewports: `desktop` (1280 × 800) and `phone`
   (390 × 844).
2. Renders it in light and in dark, the same `html.dark` switch the app
   uses.
3. Runs axe-core against the story with the WCAG 2.2 A and AA rules. Any
   violation fails the check.
4. Takes a screenshot of the story's own box and compares it with the
   baseline of the same name. Any changed pixel fails the check, apart
   from Playwright's small per-pixel colour threshold.

The story list comes from the build's `index.json`, so a new story is
checked without any other change. A story tagged `skip-visual` is left out.

## Write a story

Put the stories next to the component, in `<Component>.stories.tsx`:

```tsx
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

/** A fractional rating, with the last star clipped. */
export const Fractional: Story = { args: { value: 3.6 } };
```

- Cover each state that looks different: variants, empty, error, disabled
  and long text.
- Give stories fixed data. A story must not read the clock, call the API
  or load a remote image, or its screenshot changes from run to run.
- A whole page uses `parameters: { layout: "fullscreen" }`, which gives it
  the viewport's height as the app's `#root` does.
- Give a form control a label in the story, as the app does. axe-core
  fails an input with no accessible name.

## Run it locally

From `ui/`:

```bash
npm ci
npm run storybook                 # browse the stories at http://127.0.0.1:6006
npm run storybook:build           # the static build the checks use
npx playwright install chromium   # once per Playwright version
npm run test:visual -- --update-snapshots=all
```

The last command records screenshots into `ui/visual/baselines/` and fails
on any axe-core violation. That folder is ignored by git.

> [!IMPORTANT]
> Local screenshots are for looking, never for committing as baselines.
> Fonts, anti-aliasing and the browser build differ between machines.
> Baselines only come from the CI image.

`VISUAL_CHROMIUM_PATH` points the checks at another Chromium build, for
a host whose preinstalled browser isn't the one Playwright downloads.

## The CI job

The `UI visual and accessibility` job (`ui_visual` check) runs when a file
under `ui/` changes, and in the full suite. A change to the host that serves
the app doesn't select it, because it doesn't change what a story renders.
Like the other jobs outside `check` and `test`, it is
[deferred on a draft](../ci.md#draft-pull-requests). It runs on ready pull
requests and in the merge queue.

It runs inside the pinned Playwright image
(`mcr.microsoft.com/playwright:v<version>-noble`, pinned by digest), so every
run renders with the same browser, fonts and system libraries. The image's
version must match `@playwright/test` in `ui/package.json`. When they
differ, Playwright can't find its browser and the job fails.

The job uploads a `ui-visual` artifact with the screenshots it took and the
HTML report.

> [!NOTE]
> Until the baselines repository exists, the job records a fresh set of
> screenshots on every run instead of comparing. The accessibility checks
> still fail the run. The artifact holds the recorded set.

## Baselines

Baseline images do not live in this repository, because years of changed
PNGs would make every clone much larger. They live in a separate public
repository, `Bookclerk/bookclerk-visual-baselines`, in the same way the
built docs live in `Bookclerk/bookclerk-docs`.

> [!NOTE]
> The repository and the workflows below are planned and not built yet.

### Layout

- Each baseline set is one commit in the baselines repository, with one
  folder per viewport: `desktop/<story-id>--<theme>.png` and
  `phone/<story-id>--<theme>.png`.
- `ui/visual/baselines.lock` in this repository names the commit a
  Bookclerk commit is checked against. So every Bookclerk commit, including
  a release tag, pins the exact images it was approved with.

### How CI compares

The `ui-visual` job checks out the baselines repository at the commit in
`baselines.lock`. The repository is public, so this needs no token and
works the same on a pull request from a fork. It then sets
`CI_VISUAL_BASELINES` to that checkout, and the check compares instead of
recording.

A failing run's artifact holds every screenshot the run took, with the
expected, actual and diff image of each mismatch in the HTML report.

### How a pull request changes them

A pull request that changes how a component looks fails `ui-visual` until
its baselines are approved:

1. A reviewer opens the `ui-visual` artifact of the pull request's latest
   run and checks that the diffs are the intended change.
2. A maintainer applies the `visual-baselines` label.
3. A trusted workflow, triggered by the label, downloads that run's
   artifact. It accepts only PNG files with the expected names, sizes and
   folders, commits them as a new set to the baselines repository, and
   creates a ref for it.
4. For a branch in this repository, the workflow commits the new
   `baselines.lock` to the pull request through the API, so the commit is
   signed. For a fork, it comments the new lock line for the author to
   commit.
5. The next run compares against the new set and passes.

Because the pull request carries its own lock, the merge queue checks each
pull request against its own approved images, and two pull requests that
change baselines at once don't break each other.

### Authentication

Only the label workflow writes. It mints an installation token of the
automation App with `contents: write` on the baselines repository alone, as
the docs publisher does for `bookclerk-docs`. The `ui-visual` job itself
holds no secrets, so it runs the same for every contributor.

### Keeping the repository small

- Screenshots cover each story's own box, not the whole viewport, so most
  images are a few kilobytes. The first sets average about 22 KB per image.
- Only sets that a `main` commit or a release tag points at need to be kept.
  A scheduled cleanup can delete the refs of sets from pull requests that
  never merged.

## Next steps

- [Web UI development](README.md): run the app and the desktop window.
- [Continuous integration](../ci.md): how CI selects and runs the jobs.
