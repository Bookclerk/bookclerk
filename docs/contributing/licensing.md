# Licensing and the CLA

This page says which license covers each part of Bookclerk, what you agree
to when you contribute, how the CLA check works, and how the Bookclerk name
may be used. The reasons behind these choices are in
[ADR: Project licensing](../adr/licensing.md).

> [!NOTE]
> This page describes the project's licensing. It is not legal advice.

## Which license covers what

| Part | License |
| --- | --- |
| Everything not listed below: the hosts, helpers, built-in plugins, web app, tools and docs | [AGPL-3.0-or-later](../../LICENSE) |
| `bookclerk-plugin-sdk`, `bookclerk-plugin-abi` (including `plugin.capnp`) and `bookclerk-plugin-manifest` | MIT OR Apache-2.0 |
| The TypeScript and Python SDKs under `packages/` | MIT OR Apache-2.0 |
| The example plugins under `examples/` | MIT |
| `bookclerk-naming`, ported from Libation | GPL-3.0-only |
| `match_score` in `bookclerk-enrich`, ported from audiobookshelf | GPL-3.0-only |
| Vendored code under `third_party/` and fonts under `ui/public/fonts/` | Their own licenses, next to them |

The plugin SDKs and everything a plugin links are permissive so plugin
authors can license their plugins however they like. They use the MIT OR
Apache-2.0 dual license (the user picks either; Apache-2.0 adds an explicit
patent grant), with both texts next to each package. A plugin runs as a separate,
jailed process and talks to the host only over the plugin ABI, so the
project does not treat plugins as derivative works of the host.

Code ported from a GPL-3.0 project is a derivative of it and keeps that
license. GPL-3.0 and AGPL-3.0 both allow combining the two (section 13 of
each), so Bookclerk ships as one program. Mark a new port the same way:
set the crate's `license` field, and say in the module docs where it came
from and which license it keeps.

Every crate states its license in `Cargo.toml`. A new crate inherits
AGPL-3.0-or-later from `[workspace.package]` unless it is plugin-facing.

## What the AGPL asks of people who run Bookclerk

You can use, change, share and sell Bookclerk. If you distribute it, or let
people use a modified version over a network (for example, a hosted
Bookclerk), you must offer those users the source of your version under the
same license. Running an unmodified release for yourself or your household
asks nothing extra.

## Contributing: the CLA

Contributions are accepted under the
[Contributor License Agreement](../../CLA.md). You keep the copyright; you
give the project a broad license to your contribution, including the right
to offer it under other licenses later.

To sign:

1. Open your pull request.
2. The **CLA** check comments with a link to the agreement.
3. Read it, then post this exact comment on the pull request:

   ```text
   I have read the Bookclerk CLA and I agree to it.
   ```

4. The check records your signature and passes. You never need to sign
   again.

If someone else in the pull request has not signed, the check stays red
until they do. Comment `recheck` to run it again.

## The CLA check

The `CLA` workflow (`.github/workflows/cla.yml`) runs
`scripts/cla_check.py` on every pull request and on signature and
`recheck` comments. It checks out only the default branch and reads the
pull request through the API, so pull request code never runs with its
token.

Pull request events reach it in two steps, so the repository never needs
`pull_request_target`, which GitHub blocks by default on public
repositories:

1. The `CLA trigger` workflow (`.github/workflows/cla-trigger.yml`) runs on
   `pull_request`. It does nothing and gets a read-only token, like any
   other check on a fork pull request.
2. When it completes, the `CLA` workflow runs on `workflow_run` from the
   default branch, with the tokens it needs to comment and record
   signatures. It finds the pull request from the head commit, branch and
   repository that GitHub records for the trigger run, and takes nothing
   else from it.

A pull request that edits or deletes the trigger can only delay the check
or keep it from running, and a required **CLA** status that never arrives
keeps the pull request from merging.

> [!NOTE]
> When a fork pull request's workflows wait for a maintainer's approval
> (the repository's setting for first-time contributors), the trigger
> waits too, so the check and its comment appear once a maintainer
> approves the run. A signature comment still runs the check straight
> away.

Who must sign:

- The pull request author, the GitHub account linked to every commit, and
  every account a commit credits with a `Co-authored-by:` line. Everyone
  signs, maintainers and organization members included.
- Only tools are exempt. The project's bots (`dependabot[bot]`,
  `bookclerk[bot]` and `github-actions[bot]`) are recognized by their
  account type and login, never by the name in a commit, which anyone can
  set. The account of an AI coding assistant, whether a commit names it as
  the author or credits it as a co-author, is recognized by its address
  (`ASSISTANT_EMAILS` in the checker), which only that account can own.
  The pull request author still signs for what they submit.
- People are matched by GitHub account id, so a signature survives a
  renamed account.
- A commit whose author email belongs to no GitHub account fails the check,
  because nobody can sign for it. Add the email to your account, or amend
  the commit with an email it already has.
- A co-author whose email belongs to no account does not fail the check.
  The pull request author's signature covers only rights the author can
  grant. Section 4 of the CLA asks them to name work that is not their own,
  but naming it is not permission. Before merging such work, a maintainer
  checks that the co-author's rights were granted, or keeps the
  third-party license the work came under.
- A signature comment counts only from someone the pull request needs a
  signature from, and must be the sentence above exactly; spaces or line
  breaks around it are ignored. Every run reads the whole comment history,
  so a signature posted while another run was busy still counts.
- Each signature records the version of the CLA it accepts (`claVersion`),
  so a later edit to `CLA.md` does not blur what earlier signers agreed to.
- The check runs when a pull request is opened, pushed to, reopened or
  closed, and when it moves to another base branch, because the new base can
  bring in commits from other authors without changing the head commit.

The result is the **CLA** commit status on the pull request's head commit,
and one comment that lists who still has to sign, edited in place. Merge
queue runs report a passing **CLA** status, because each pull request
passed it to enter the queue. A pull request with more than 250 commits
cannot be checked and fails closed.

The status belongs to the commit, not to the pull request. When other open
pull requests have the same head commit (from another author, or against
another base), the status passes only when every one of them passes, so
nobody can borrow a signed contributor's result. When one of them is
closed, the others decide the status again. When one is pushed to, the
commit it left keeps its last status until one of the others is checked
again; comment `recheck` there. More than 10 such pull requests fail
closed.

> [!NOTE]
> Anyone can open a pull request on the same head commit as yours and turn
> your check red. The check's comment names that pull request. A maintainer
> closes it, and the status is decided again without it; comment `recheck`
> if it is not.

Signatures are recorded in `signatures/cla.json` on the `cla-signatures`
branch, so recording one needs no pull request and no bypass of the main
branch rules. The copy on `main` stays empty and only seeds the branch.
Each signature is one commit made through the API by the
[automation App](automation.md#automation-app), so GitHub signs it
for the App and nothing else needs to push there.

Setup, once, by a repository administrator:

1. Create the `cla-signatures` branch from `main`, so it starts with
   `main`'s empty `signatures/cla.json`. The check fails until the file
   exists.
2. Make sure the automation App's secrets are set (see
   [the automation App](automation.md#automation-app)). Without them
   the check still decides, but it cannot record a new signature and
   reports an error instead.
3. Add two branch rulesets targeting `cla-signatures`. A bypass exempts
   an actor only from the ruleset that lists it, so splitting the rules
   lets the App add commits while nobody, the App included, can rewrite
   them:
   - **Restrict creations** and **Restrict updates**, with only the
     automation App on the bypass list, so only the App can add commits.
   - **Restrict deletions**, **Block force pushes**, **Require linear
     history** and **Require signed commits**, with an empty bypass list.
     The App's commits already meet these, because each is one
     GitHub-signed commit on top of the branch.
4. After the check has run on a pull request, add the **CLA** status to the
   required checks in the main branch ruleset, with **GitHub Actions** as
   its expected source, so no other app that can post statuses can satisfy
   it. The `CLA check` job is only the workflow that reports it.

## The Bookclerk name and logo

None of the licenses grants trademark rights. The name, logo and mark are
covered by the [trademark policy](../../TRADEMARK.md), which the AGPL allows
as an additional term under section 7(e). In short: talk about Bookclerk and
redistribute unmodified releases freely, and give forks and modified hosted
versions a different name.

## Next steps

- Read the [Contributor License Agreement](../../CLA.md) before your first
  pull request.
- Read the [trademark policy](../../TRADEMARK.md) before naming a plugin,
  fork or service.
- See [Plugins](../plugins/README.md) to start building on the MIT OR
  Apache-2.0 SDKs.
