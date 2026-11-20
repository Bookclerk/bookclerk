# ADR: What needs the operator, and what an Owner controls

- **Status:** Accepted
- **Date:** 2026-11-19
- **Related:** [First-party identity](first-party-identity.md),
  [Patron scopes and linked sign-in](patron-scopes.md),
  [Control plane](control-plane.md)

## Context

Bookclerk has one operator per install (the operator token, or an Owner who
has elevated) and any number of Owners, Administrators and Members. The
split between the operator and an Owner exists so that actions critical to
the health of a branch need a deliberate step away from the Owner's
everyday session: elevation, with a password, a passkey or a provider
step-up, for a short time.

Most actions already followed that idea. Settings, plugins, network
grants, database moves, joining or leaving a library, assigning the Owner
role and impersonation all need the operator. A few critical actions still
ran on an Owner's everyday session, some behind a fifteen-minute
"recent sign-in or current password" check:

- removing a shared branch;
- creating, changing, rotating or deleting Bookclerk-as-identity-provider
  clients;
- the second-factor policy;
- upstream sign-in providers.

Administrators' powers were fixed by role. An Owner could change an
Administrator's role, disable or delete them, but could not narrow what
one Administrator may do.

## Decision

1. **The rule.** An action needs the operator when it changes what code
   runs or what it can reach, who can sign in or hold keys, destroys
   something hard to rebuild, or acts as another person. Everything about
   people and sharing that an Owner does week to week stays on the Owner's
   everyday session.
2. **Four actions move to the operator.** Removing a shared branch
   (`DELETE /api/config/nodes/{node_id}`), changing identity-provider
   clients (`POST`/`PUT`/`DELETE /api/auth/oidc/clients…` and
   `rotate-secret`), the second-factor policy (`PUT /api/auth/mfa-policy`)
   and upstream sign-in providers (`PUT /api/auth/oidc/config`). Each
   route sits in the operator-only route group, so the auth layer refuses
   any library session before the handler runs. The fifteen-minute
   `current_password` step-up on them is gone. An Owner still reads the
   clients, the policy and the providers.
3. **Administrator switches.** Seven switches, each with a library default
   and a per-Administrator override, set only by an Owner or the operator,
   all on by default: manage members, set patron limits, library-wide scan
   and acquire, run the job queue, fix the catalog, enroll hosts, and
   approve interlibrary requests. The member controls (link store
   accounts, download, edit metadata, upload, request books, see prices
   and store links) also apply to Administrators, set only by an Owner or
   the operator.
4. **Elevation stays short-lived**, as before.

## Consequences

- With the second-factor policy and the sign-in providers behind
  elevation, an Owner who can't elevate cannot weaken the checks that
  guard elevation.
- An Owner without elevation sees **Settings → Sign-in** read-only and
  cannot remove a shared branch; the web app says to elevate under
  **Account**.
- Scripts that changed these settings with an Owner's session must use
  the operator token or an elevated session instead.
- The Administrator switches reuse the member controls' shape: a library
  document, an overrides table and a check inside the write batch. They
  land in their own change after this one.
