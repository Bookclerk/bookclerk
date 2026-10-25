# ADR: Security advisories are drafted only for what Bookclerk ships

- **Status:** Accepted
- **Date:** 2026-10-22
- **Related:** [Untrusted-input fuzzing](untrusted-input-fuzzing.md),
  [Continuous integration](../contributing/ci.md#osv-advisories),
  [Fuzz-crash triage](../contributing/ci.md#fuzz-crash-triage),
  [OSV remediation](../contributing/ci.md#osv-remediation)

## Context

Scheduled scans find two kinds of problems: crashes in Bookclerk's own code
(the daily fuzzing run) and advisories against locked dependencies (the OSV
scan of every lockfile, and Dependabot). The OSV remediation workflow already
bumps a vulnerable dependency when a compatible fix exists. What it does not
answer is whether anyone running Bookclerk is affected, which is the question
a repository security advisory exists to answer.

Most dependency alerts are not Bookclerk vulnerabilities. The repository holds
lockfiles for reference examples, fuzzing, CI tooling and development
dependencies, none of which ships; and a crate can be locked without its
vulnerable code ever being linked into a binary. Filing an advisory for every
alert would bury the real ones, and not filing any would leave users of a
release without notice.

Everything a workflow writes to logs or artifacts of this public repository is
public, while code-scanning results, Dependabot alerts and draft advisories
are not.

## Decision

1. **One rule for every source.** A draft repository security advisory is
   filed only when a shipped Bookclerk artifact contains the vulnerable code,
   or Bookclerk's own shipped code has the bug. Fuzz-crash triage and the OSV
   advisory workflow both apply it.
2. **Shipped artifacts are declared, not guessed.**
   [`shipped-artifacts.toml`](../../shipped-artifacts.toml) lists each
   artifact (host binaries with the platform and optional guests and the web
   UI, the Rust, TypeScript and Python plugin SDKs, the diagnostics collector
   Worker), its advisory ecosystem and package, and which lockfile entries end
   up inside it. Every lockfile in the repository is either listed there or
   named as not shipped, and a test fails otherwise.
   [`fuzz/boundaries.toml`](../../fuzz/boundaries.toml) names the same
   artifacts for fuzz targets.
3. **Evidence per ecosystem, strongest available.**
   - Rust: the crate must be in the shipped dependency closure of the
     shipped crates (normal edges on every target platform; no dev or
     proc-macro edges, and build dependencies only when they compile vendored
     C into the binary, the `*-src` crates). When
     the upstream advisory names vulnerable functions, the shipped binaries
     are built with fat LTO and debug info, the method OSV-Scanner's Rust call
     analysis uses, and the advisory is filed only if a named function is in
     their DWARF. Without function data, or for a library such as the Rust
     SDK whose code runs in third-party plugins, the closure alone files the
     advisory and its text says reachability is unproven.
   - npm: the package must be a production dependency; for the web UI it must
     also have code in the production bundle (from the bundle's source maps),
     so tree-shaken packages do not count. Function-level reachability does
     not exist for npm, and the advisory says so.
   - Python: the SDK has no runtime dependencies, so only its own code can be
     affected. A test and the workflow both fail if one is added before its
     closure is resolved here.
   - Not shipped, never an advisory: examples, fuzzing, scripts, GitHub
     Actions and every development dependency. Their alert and the
     remediation bump are the whole record.
4. **Drafts before the first release too.** A draft is private to
   maintainers and publishes nothing, so both workflows file one as soon as
   shipped code is affected, release or not: it proves the filing path end to
   end, and a fix landed before the first release never ships vulnerable.
   Until a release is published (prereleases count, drafts do not), a draft
   proposes no affected range and says so; a maintainer closes it once the fix is on `main`, or sets the range
   when a release ships the code. Failed fuzz oracles (bugs in a check, not
   vulnerabilities) become issues instead. The advisory workflow also writes a
   verdict report for every alert, sealed with the passphrase maintainers
   keep, as an artifact of its run.
5. **Drafts, never publication (for now).** The automation files drafts, which
   are private to maintainers, with a proposed affected range ending at the
   newest release (or the SDK's published version), the upstream ids, CWE and
   CVSS, and the evidence. A maintainer confirms the range and severity, prepares
   code fixes in a temporary private fork, sets the patched version and
   publishes. Each draft carries a hidden marker per upstream id and Bookclerk
   package (or per crash signature), so a draft is never filed twice, and one
   a maintainer closed as not affecting Bookclerk is not filed again. When a
   later run finds the same problem, an open draft is updated instead: only
   the part of its description the automation owns is rewritten (and an
   unset affected range filled once a release exists), so a maintainer's
   notes, severity and range survive. Closed and published advisories are
   never changed.
6. **Least privilege, nothing public.** The job that runs build scripts and
   build tooling has no secrets and no token, and its output names no
   advisory. Alerts are read in another job with a read-only token that is
   revoked as soon as they are read; drafts are filed from a job that builds
   nothing, with an advisory-write token minted only for that job.
   Alert data travels between jobs sealed, and logs print counts only.
7. **Fail closed.** A call analysis that cannot run, or whose output does not
   look like a real analysis, files the advisory on the closure alone; a web
   bundle that cannot be built falls back to the production dependencies.

## Future: cutting the fix release automatically

Not built yet, because Bookclerk has no release. The goal, once releases
exist, is to close the loop from alert to published fix:

1. When a remediation bump or a fix pull request lands on `main` for an
   advisory whose draft is open, a release workflow cuts a patch release
   from `main` (version bump, changelog entry naming the advisory ids that
   are already public upstream, signed tag, the release build and packaging
   of every shipped artifact, SDK packages to their registries).
2. The workflow sets the draft's patched version to the new release and
   links it, so publishing the advisory and the release happens together.
3. Publishing becomes automatic where it can be: the advisory and the fix
   release are published together without a maintainer when the evidence is
   unambiguous (an upstream advisory with a fixed version that the remediation
   bump took, a passing release build, and a verdict the maintainers have
   allowed to auto-publish). Anything else (own-code crashes, call analysis
   that failed closed, no upstream fix) stays a maintainer action: the
   workflow prepares, a maintainer publishes. Requesting a CVE stays manual
   until then.
4. The affected range is then checked, not proposed: the same evidence is
   recomputed against each released tag, so the draft names the first and
   last affected releases instead of "every release up to the newest".

Prerequisites: release packaging for all artifacts, a versioning policy for
the host and the SDKs, and registry publishing credentials scoped to the
release workflow.

## Consequences

- An advisory for Bookclerk means the vulnerable code ships; reviewers spend
  their time on drafts that matter.
- A false negative needs a crate outside the declared closure, or vulnerable
  functions absent from the LTO build while present at run time. The closure
  is the union of every platform and feature the workspace resolves, and a
  failed analysis falls back to the closure.
- The call-analysis build takes hours, so it runs only when an advisory with
  function data for a shipped crate has no advisory yet or only an open draft
  (which the analysis then refreshes), and only on the weekly scheduled scan
  or a manual run. Until then such an advisory waits rather than being filed.
- Tampering by a compromised dependency's build script is out of reach of
  this design: it can make the analysis job report less than the truth. The
  closure and the plan come from a job that runs no third-party code, and a
  missing or implausible analysis files on the closure.
- New shipped artifacts or lockfiles must be added to
  `shipped-artifacts.toml` in the pull request that adds them.
