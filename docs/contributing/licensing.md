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
| `bookclerk-plugin-sdk`, `bookclerk-plugin-abi` (including `plugin.capnp`) and `bookclerk-plugin-manifest` | MIT |
| The TypeScript and Python SDKs under `packages/` | MIT |
| The example plugins under `examples/` | MIT |
| `bookclerk-naming`, ported from Libation | GPL-3.0-only |
| `match_score` in `bookclerk-enrich`, ported from audiobookshelf | GPL-3.0-only |
| Vendored code under `third_party/` and fonts under `ui/public/fonts/` | Their own licenses, next to them |

The plugin SDKs and everything a plugin links are MIT so plugin authors can
license their plugins however they like. A plugin runs as a separate,
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
to offer it under other licenses later. In return the project promises your
contribution stays available under the license it was submitted under.

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

The `CLA` workflow (`.github/workflows/cla.yml`) runs the CLA Assistant Lite
action on every pull request and on signature comments. It reads pull
request metadata only and never checks out pull request code.

Signatures live in `signatures/cla.json` on the `cla-signatures` branch, not
on `main`, so recording one needs no pull request and no bypass of the main
branch rules. The maintainers and the project's bots are on the allowlist.

Setup, once, by a repository administrator:

1. Create the `cla-signatures` branch from `main`. The action creates the
   signature file on the first signature.
2. Make sure no ruleset blocks the `github-actions[bot]` from pushing to
   `cla-signatures`.
3. After the check has run on a pull request, add **CLA** to the required
   checks in the main branch ruleset. The workflow also listens to
   `merge_group`, so queued pull requests report it too.

## The Bookclerk name and logo

The licenses cover copyright only. The name, logo and mark are covered by
the [trademark policy](../../TRADEMARK.md), which the AGPL allows as an
additional term under section 7(e). In short: talk about Bookclerk and
redistribute unmodified releases freely, and give forks and modified hosted
versions a different name.
