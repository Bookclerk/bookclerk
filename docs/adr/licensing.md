# ADR: Project licensing

- **Status:** Accepted
- **Date:** 2026-10-27
- **Related:** [Licensing and the CLA](../contributing/licensing.md),
  [Plugin capabilities v3](plugin-capabilities-v3.md)

## Context

Bookclerk was published under GPL-3.0-or-later, but the root `LICENSE` held
only the short "how to apply" notice and a link to the license text.
GitHub's license detector compares `LICENSE` with the canonical text, found
no match and reported the license as "Other".

The owner also wanted to protect the Bookclerk name, keep the option to file
patents, and considered blocking commercial use. Reviewing those goals
showed:

- No open source license grants trademark rights. GPL-3.0 and AGPL-3.0
  section 7(e) let a project add a term declining them; a brand policy does
  the rest. BSD-3-Clause's name clause only forbids using contributors'
  names to endorse derived products.
- No license limits the owner's own patent filings. GPL-3.0 and AGPL-3.0
  section 11 license users under contributors' patents that read on their
  contributions.
- No OSI-approved license forbids commercial use. Non-commercial licenses
  (PolyForm Noncommercial, Commons Clause) are not open source, are not
  detected by GitHub, keep the software out of distributions, and could not
  apply anyway: `bookclerk-naming` and `bookclerk-enrich`'s match scoring
  are ported from GPL-3.0 projects (Libation and audiobookshelf).
- Bookclerk is mostly a daemon with a web app. GPL-3.0 lets someone host a
  modified Bookclerk for others without sharing their changes; AGPL-3.0
  closes that gap and stays open source.
- The TypeScript and Python plugin SDKs were MIT, but the Rust plugin SDK,
  ABI and manifest crates inherited GPL-3.0-or-later, which would have
  pushed the GPL onto Rust plugins.
- Every past commit has one human author, so relicensing needs no one
  else's consent. That stops being true once outside contributions land
  without an agreement.

## Decision

1. The project is licensed under **AGPL-3.0-or-later**. `LICENSE` holds the
   verbatim AGPL-3.0 text so GitHub detects it; the copyright line and the
   "or later" notice live in the README's License section.
2. The code plugins build on is **MIT OR Apache-2.0**: the TypeScript and
   Python SDKs, `bookclerk-plugin-sdk`, `bookclerk-plugin-abi` and
   `bookclerk-plugin-manifest`. The example plugins are **MIT**.
3. Ports of GPL-3.0 code keep **GPL-3.0-only**: `bookclerk-naming` and
   `bookclerk-enrich`'s `match_score`. Section 13 of both licenses allows
   combining them with the AGPL code.
4. The name and logo are covered by [`TRADEMARK.md`](../../TRADEMARK.md),
   added to the license as a section 7(e) term in the README.
5. Contributions are accepted under a **Contributor License Agreement**
   ([`CLA.md`](../../CLA.md)), checked on every pull request by the `CLA`
   workflow. Everyone signs it, maintainers and organization members
   included; only bots and the accounts of AI coding assistants, as commit
   author or co-author, are exempt. It grants the project a license to relicense contributions
   without asking each contributor again, and deliberately makes no
   promise to keep a contribution under the license it was submitted
   under, so the maintainers can change the project's licensing without
   tracking down or rewriting small contributions.
6. The project does not restrict commercial use.

## Consequences

- GitHub shows the repository as AGPL-3.0.
- Anyone who hosts a modified Bookclerk for others must offer them its
  source. Unmodified self-hosting is unaffected.
- Plugin authors choose their own license.
- First-time contributors sign the CLA once before their pull request can
  merge; some people decline to sign CLAs.
- The CLA keeps a later dual-license or relicense possible without tracking
  down every contributor. Code ported from other projects is not covered by
  it and keeps its own license.
- The trademark policy, the CLA wording and any future registration of the
  name should be confirmed with a lawyer.
