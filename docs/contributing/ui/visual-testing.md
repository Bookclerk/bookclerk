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

Each run first records a full set of screenshots, then, once a set is
approved, compares with it. The job uploads a `ui-visual` artifact:
`baselines/` holds the recorded set, `record/` and `compare/` the HTML
reports, with the expected, actual and diff image of each mismatch.

> [!NOTE]
> While `ui/visual/baselines.lock` says `none`, the job only records. The
> accessibility checks still fail the run.

## Baselines

Baseline images do not live in this repository, because years of changed
PNGs would make every clone much larger. They live in a separate public
repository, `Bookclerk/bookclerk-visual-baselines`, in the same way the
built docs live in `Bookclerk/bookclerk-docs`.

### Layout

- Each baseline set is one commit in the baselines repository, with one
  folder per viewport: `desktop/<story-id>--<theme>.png` and
  `phone/<story-id>--<theme>.png`.
- `ui/visual/baselines.lock` in this repository names the commit a
  Bookclerk commit is checked against (or `none`). So every Bookclerk
  commit, including a release tag, pins the exact images it was approved
  with.
- `pending/pr-<number>` holds the set a pull request proposes;
  `approved/<commit>` tags keep every approved set.

### How CI compares

The `ui-visual` job reads the lock with `scripts/visual_baselines.py lock`
and checks the baselines repository out at that commit. The repository is
public, so this needs no token and works the same on a pull request from a
fork. It then sets `CI_VISUAL_BASELINES` to that checkout, and the check
compares after recording.

### How a pull request changes them

A pull request that changes how a component looks fails `ui-visual` until
its new screenshots are approved. The `Visual baselines` workflow
(`.github/workflows/visual-baselines.yml`) handles the rest:

1. When CI finishes, the workflow reads the run's recorded set. If it
   differs from the set the pull request's lock names, it pushes the set to
   `pending/pr-<number>` and posts one comment on the pull request. The
   comment shows the approved and proposed image of each changed
   screenshot, and an approval checkbox for that head commit.
2. An approver ticks the checkbox. Ticking needs write access, so a visual
   reviewer without it replies `/approve-visuals` instead.
3. The workflow checks the approver and that the comment is still for the
   pull request's head. It then tags the set `approved/<commit>` and commits
   the new lock to the pull request's branch through the API, so GitHub
   signs the commit. For a fork, the comment gives the author the lock line
   to commit.
4. The next run compares against the new set and passes.

A tick from anyone else is undone with the reason. A push after the
comment makes it stale: the approval is refused, and the next CI run
updates the comment for the new head.

Because the pull request carries its own lock, the merge queue checks each
pull request against its own approved images, and two pull requests that
change baselines at once don't break each other.

### Who can approve

- Anyone with the admin or maintain role on this repository.
- Any active member of the `visual-reviewers` organization team. The team
  grants nothing else, so it can hold a designer without code access.

### Safety

The workflow runs from the default branch through `workflow_run` and
`issue_comment`, and never checks out or runs pull request code. The
recorded set comes from a run of the pull request's own code, so it is
untrusted. `scripts/visual_baselines.py` accepts only PNG files with the
expected names, folders and sizes, and only a comment edit that ticks the
box and changes nothing else (see
[Fuzzing](../fuzzing.md#visual-baselines-python)).

The `ui-visual` job holds no secrets. The workflow's App tokens are
narrowed per step: Contents on the baselines repository alone for sets and
tags, and Pull requests, Contents and organization Members (read) on this
repository for the comment, the lock and the team check.

### Setup

Before the first approval, an organization owner:

1. Creates the public repository `Bookclerk/bookclerk-visual-baselines`,
   with a ruleset that blocks deleting or force-pushing `approved/*` tags.
2. Installs the automation App on it with Contents read and write.
3. Grants the App the organization Members (read) permission.
4. Creates the `visual-reviewers` team and adds the approvers.

The lock stays `none` until the first set is approved.

### Keeping the repository small

- Screenshots cover each story's own box, not the whole viewport, so most
  images are a few kilobytes. The first sets average about 22 KB per image.
- Only sets that an `approved/*` tag points at need to be kept. Pending
  refs are overwritten on each proposal, so a pull request that never
  merges leaves at most one pending set behind.

## Next steps

- [Web UI development](README.md): run the app and the desktop window.
- [Continuous integration](../ci.md): how CI selects and runs the jobs.
