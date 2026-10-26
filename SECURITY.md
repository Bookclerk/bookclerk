# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue:

1. **GitHub private reporting (preferred).** Open the repository's
   **Security** tab and choose **Report a vulnerability**. The report stays
   visible only to you and the maintainers.
2. **Email.** If you cannot use GitHub, write to
   [contact@bookclerk.app](mailto:contact@bookclerk.app).

Include what you found, the version or commit, and the steps or input that
show it. A minimal reproduction helps most. You will get an answer once the
report has been read; please give us a reasonable time to fix it before you
disclose it publicly.

## Supported versions

Bookclerk has no release yet. Only the `main` branch is supported, and fixes
land there.

## How fixes are published

Until the first release, a security bug is fixed in the open: the fix is an
ordinary pull request that states the problem plainly, and no security
advisory is published for it, because there is no released version to warn
users of. Once releases exist, fixes to released versions come with a GitHub
security advisory naming the affected and fixed versions; see
[ADR: security advisory automation](docs/adr/security-advisory-automation.md).

## Scope

The security model, and what Bookclerk does and does not defend against, is
described in the [security checklist](docs/server/security.md), the
[guest jail](docs/internals/guest-jail.md) and the
[untrusted input ADR](docs/adr/untrusted-input-fuzzing.md).
