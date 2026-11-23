# Visual and accessibility testing

CI checks every web page Bookclerk ships for layout changes and
accessibility problems: the web app's pages and components, the website
(`tools/site`) and the diagnostics collector's pages
(`tools/diagnostics-collector`). CI opens every Storybook story, every web
app page and every Worker page in a browser, checks it with axe-core, and
compares a screenshot with an approved baseline image. It also fails when a
web app component never appears in any of them.

Nothing here ships in the app or the Workers. Storybook, Playwright, the
stories, the sample API answers and the Workers' local page servers are
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

## Website and collector pages

`ui/visual/workers.spec.ts` checks the pages the two Cloudflare Workers
serve, in the same run and at the same viewports. Each page is opened in
light and in dark (both follow `prefers-color-scheme`), must answer with its
expected status, passes the same axe-core rules, and is compared with a
baseline of its whole length, named `<worker>--<page>--<theme>.png`.

- **Website**: `tools/site/audit/serve.ts` serves the built site through its
  real Worker under `wrangler dev`, as the [website audit](../automation.md#website-checks)
  does. The pages are `VISUAL_PAGES` in `tools/site/audit/pages.ts`: the
  home, download, privacy and security pages, the 404 and
  docs-unavailable pages, and one docs page. That docs page is the fixture
  `tools/site/audit/visual-docs/layout.md`, which holds one of each kind of
  docs block. Real docs pages aren't screenshotted, because their words
  change far more often than their layout. CI builds the site from a fixed
  commit and date, and the homepage's catalog sample is fixed, so nothing
  but the layout changes the images.
- **Collector**: `tools/diagnostics-collector/visual/serve.ts` renders every
  page template in `dashboard/pages.ts` from fixed sample data and a fixed
  clock, with the dashboard's static files. No database, GitHub App or
  session is involved. The pages are `VISUAL_PAGES` in
  `tools/diagnostics-collector/visual/pages.ts`; the server refuses to
  start when that list and its renderers disagree.

Each Worker also lists its building blocks beside its pages, as
`VISUAL_COMPONENTS` in the same file: the website's header, footer, hero,
buttons, cards, callouts, code blocks and tables, and the collector's
header, stat cards, charts, tables, forms and event log. Each one is taken
from a page that shows it (the first element matching its CSS selector),
checked with axe-core on its own and compared with a screenshot of just
that element, named `<worker>--component--<name>--<theme>.png`. A change to
one building block then shows up as its own small image beside the full
pages.

While a building block is captured, `ui/visual/sticky-headers-static.css`
keeps the website's sticky header in the page flow. Otherwise a block
taller than the viewport, such as the hero on a phone, is drawn with the
header pinned over part of it, wherever Playwright scrolled to.

A new collector page template gets a sample render in `visual/serve.ts`
and an entry in `visual/pages.ts`. A new website page gets an entry in
`audit/pages.ts`. A new building block gets a `VISUAL_COMPONENTS` entry.

## Web app pages

`ui/visual/app.spec.ts` opens the web app itself, built without
minification (`npm run build:visual`, into `ui/visual-dist/`), at the same
viewports and in light and dark. No daemon runs behind it:

- Every `/api/` request is answered from a small sample library in
  `ui/visual/app/fixtures.ts`: public-domain books on two storefronts, an
  Owner, two patrons, a branch and a few plugins, seen at a fixed time, so
  relative dates read the same on every run. Covers and store icons are
  generated plain tiles; no store artwork is used.
- Each JSON answer, an error answer included, is checked against the
  response the daemon's OpenAPI document (`crates/bookclerkd/openapi.json`)
  gives for its status, by `ui/visual/app/openapi.ts`. A request the samples
  don't answer, a path or status the document doesn't list, or an answer the
  document doesn't allow fails the test and is named, so the samples can't
  drift from the real API.
- Before axe-core and the screenshot, the test waits until no API request
  has started for half a second, so a tab or dialog that loads its data
  after opening is shown with it.
- The live-updates socket is held open without messages.

The pages are `PAGES` in `app.spec.ts`: sign-in, Discover, Library,
Wishlist, Accounts, Status and every Settings tab, plus dialogs and states
that open from a page (book details, the player, plugin consent, adding a
branch) or appear only in some states (a fresh install's welcome screen, a
required second factor, the join and library card pages). A page that needs
a different library names a scenario from `SCENARIOS` in `fixtures.ts`,
which answers a few requests differently. Each page is checked with
axe-core and compared with a screenshot named `app--<page>--<theme>.png`.

A new page, or a state a page shows only sometimes, gets a `PAGES` entry,
with a scenario when the default samples don't produce it.

## Coverage

The recording run in CI sets `VISUAL_COVERAGE_CHECK=1`. The suite then
notes which web app components put something on the page in each story and
app page, using a stand-in for the React DevTools hook (`ui/visual/coverage.ts`).
A dialog that is mounted but closed draws nothing and doesn't count. When the
run ends, `ui/visual/coverage-check.ts` fails it if any component exported
from a module under `ui/src/components/` never rendered, and names each one.
It reads exports declared inline (`export function Name`) and in export lists
(`export { Name }`); `coverage-check.spec.ts` checks that reading.

The builds the suite opens keep function names (they aren't minified), so a
component is known by its name.

To cover a new component, open it from an app page (add a `PAGES` entry or
a scenario), or write a story when no page in a browser reaches it (the
desktop window's own first-run step, say). Remove a component nothing uses
rather than covering it.

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
- Don't let a story read the page's own address either. Storybook serves
  every story from `/iframe.html`, so pass a page such as `NotFoundPage`
  the path to show.
- Render the component as the app lays it out, not a layout made up for
  the story. A component that reads shared app state (the page's player,
  say) exports a view that takes that state as props, such as
  `PlayerBarView`, and the story renders the view.
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
npm run build:visual              # the unminified app build the app pages use
npx playwright install chromium   # once per Playwright version
npm run test:visual -- --update-snapshots=all
```

The last command records screenshots into `ui/visual/baselines/` and fails
on any axe-core violation. That folder is ignored by git.

The run also starts the Workers' page servers, so install and build them
first:

```bash
(cd ../tools/site && npm ci && SITE_COMMIT=0000000 SITE_BUILT_ON=2026-01-01 npm run build)
(cd ../tools/diagnostics-collector && npm ci)
```

Add `app`, `workers` or `stories` after `--` to run only one of the specs.
Set `VISUAL_COVERAGE_CHECK=1` on a run of all of them to check coverage as
CI does, and `VISUAL_COVERAGE_REPORT=1` as well to list every component and
whether it rendered.

> [!IMPORTANT]
> Local screenshots are for looking, never for committing as baselines.
> Fonts, anti-aliasing and the browser build differ between machines.
> Baselines only come from the CI image.

`VISUAL_CHROMIUM_PATH` points the checks at another Chromium build, for
a host whose preinstalled browser isn't the one Playwright downloads.

## The CI job

The `UI visual and accessibility` job (`ui_visual` check) runs when a file
under `ui/`, `tools/site/`, `tools/diagnostics-collector/` or
`assets/brand/` changes, or `SECURITY.md` (the website's security page), or
the daemon's OpenAPI document (`crates/bookclerkd/openapi.json`, which the
app pages' samples are checked against), and in the full suite. Any other
change to the host that serves the app doesn't select it, and neither does
a change under `docs/`, because neither changes what the checks render.
Like the other jobs outside `check` and `test`, it is
[deferred on a draft](../ci.md#draft-pull-requests). It runs on ready pull
requests and in the merge queue.

It runs inside the pinned Playwright image
(`mcr.microsoft.com/playwright:v<version>-noble`, pinned by digest), so every
run renders with the same browser, fonts and system libraries. The image's
version must match `@playwright/test` in `ui/package.json`. When they
differ, Playwright can't find its browser and the job fails.
Dependabot bumps `@playwright/test` but can't move the image, so its
Playwright pull request fails this job until the image's tag and digest
(and the `playwright-core` override in `ui/package.json`) move to the same
version in that pull request. A new browser can render some screenshots
differently; approve those in the visual comment as usual.

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
  folder per viewport: `desktop/<id>--<theme>.png` and
  `phone/<id>--<theme>.png`, where the id is a story id, `app--<page>`, or
  a Worker page or building block.
- `ui/visual/baselines.lock` in this repository names the commit a
  Bookclerk commit is checked against (or `none`). So every Bookclerk
  commit, including a release tag, pins the exact images it was approved
  with.
- `pending/pr-<number>` holds the set a pull request proposes;
  `approved/<commit>` tags keep every approved set.

### How CI compares

The `ui-visual` job reads the lock with `scripts/visual_baselines.py lock`
and first checks that the commit carries an `approved/<commit>` tag. Only
the approval workflow creates those tags, so a lock that names a pending
set, copied by hand, fails the job instead of skipping the approval. It
then checks the baselines repository out at that commit. The repository is
public, so this needs no token and works the same on a pull request from a
fork. It sets `CI_VISUAL_BASELINES` to that checkout, and the check
compares after recording.

The `Visual baselines` workflow compares against the same lock CI used:
the one in the pull request merged into its base branch. A pull request
that leaves the lock alone is compared with the base branch's approved set,
so it only gets a comment when its own change alters a screenshot. Without
a merge ref (the pull request conflicts with its base), the head's lock
stands in when it names an approved set, and otherwise the base branch's
lock does, so a branch started before the first approval isn't shown every
image as new. Merging the base branch in still gives the exact comparison,
and lets CI run on the merged code. Like CI, the workflow treats an
untagged lock as no set at all, so every image needs approval. The comment
names the approved set it compared with.

### How a pull request changes them

A pull request that changes how a component looks fails `ui-visual` until
its new screenshots are approved. The `Visual baselines` workflow
(`.github/workflows/visual-baselines.yml`) handles the rest:

1. When CI finishes, the workflow reads the run's recorded set. If it
   differs from the approved set CI compared with, it pushes the set to
   `pending/pr-<number>` and posts one comment on the pull request. The
   comment opens with how many screenshots are changed, new and removed,
   and the approval checkbox. Below that, each category is a collapsed
   section: changed screenshots show the approved and proposed image, new
   ones only the proposed image and removed ones only the approved image.
   Every screenshot is listed. When a set is too large for one GitHub
   comment (65,536 characters), the last rows show only the name. Their
   images go in follow-on comments headed "Visual changes, continued",
   which the workflow edits, adds or deletes along with the main comment.
   The workflow then
   requests a review from the `visual-reviewers` team, so its members are
   notified.
2. An approver ticks the checkbox. Ticking needs write access, so a visual
   reviewer without it replies `@bookclerk approve-visuals` instead, the
   same form as the other bot commands.
3. The workflow checks the approver and that the pull request is open. It
   then tags the set `approved/<commit>` and commits
   the new lock to the pull request's branch through the API, so GitHub
   signs the commit. For a fork, the comment gives the author the lock line
   to commit.
4. The next run compares against the new set and passes.

A tick from anyone else is undone with the reason. The approval is of the
images the comment shows, not of a commit, so a push after the comment
doesn't void it. Later runs compare with the approved set, so screenshots
that haven't changed since stay approved. Ones that changed need approval
again, in a new comment; the approved comment stays as the record. Until a
comment is approved, a run that records different images updates it in
place, and a run that records the same images leaves it alone.

Because the pull request carries its own lock, the merge queue checks each
pull request against its own approved images. Two pull requests that both
approve new images change the same lock line, so the second one to merge
conflicts. Merging the base branch into it resolves that: CI then compares
with the newly approved set, and the comment shows only that pull
request's own changes.

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
repository for the comment, the review request, the lock and the team
check.

### Setup

Before the first approval, an organization owner:

1. Creates the public repository `Bookclerk/bookclerk-visual-baselines`
   with two rulesets, each with the automation App as its only bypass
   actor (bypass mode **Always**):
   - **All branches**: restrict creations, updates and deletions, and
     block force pushes. Only the App writes the `pending/pr-<number>`
     branches.
   - **All tags**: the same rules. Only the App creates `approved/*`
     tags, and it never moves or deletes one.

   The App never deletes a ref, so these rules cost the workflow nothing.
2. Installs the automation App on it with Contents read and write.
3. Grants the App the organization Members (read) permission.
4. Creates the `visual-reviewers` team, adds the approvers, and gives the
   team Read access to this repository. GitHub only accepts a review
   request for a team that can see the repository; Read grants no code
   write.

The lock stays `none` until the first set is approved. Until then every
pull request that touches `ui/` is compared with an empty set and its
comment shows every screenshot as new. Approving one of them and merging
it sets the lock on the default branch, and later pull requests are
compared with that set.

### Keeping the repository small

- Screenshots cover each story's or building block's own box, not the whole
  viewport, so most images are a few kilobytes. The first sets average about 22 KB per image.
- Only sets that an `approved/*` tag points at need to be kept. Pending
  refs are overwritten on each proposal, so a pull request that never
  merges leaves at most one pending set behind.

## Next steps

- [Web UI development](README.md): run the app and the desktop window.
- [Continuous integration](../ci.md): how CI selects and runs the jobs.
