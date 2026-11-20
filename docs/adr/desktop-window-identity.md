# ADR: The desktop window is the person signed in to the computer

- **Status:** Accepted
- **Date:** 2026-11-20

## Context

The desktop window runs the host in-process and serves the web app over its
own custom scheme, with no listener (`bookclerkd::embedded`). Until now it
added the operator token to every request and dropped every cookie, so the
window was always the operator. It never opened as a patron, had no
elevation step, and could not impersonate, because elevation and
impersonation work by setting a session cookie.

An earlier plan was for the window to open as the library's Owner. That
assumes the person at the computer is the Owner, which stops being true as
soon as a second person uses the app on the same library, or the Owner role
moves. Desktop apps usually take a different approach: the operating system
already knows who is signed in, and each OS account keeps its own settings
in its own configuration directory.

## Decision

1. **The OS account is the identity.** The window acts as the person signed
   in to the computer's OS account. Each OS account has its own files
   directory already (the platform data directory), and now its own profile.
2. **A per-account profile names the patron.** `profile.toml` in the OS
   account's standard configuration directory (`$XDG_CONFIG_HOME/bookclerk`
   on Linux, `~/Library/Application Support/app.Bookclerk.bookclerk` on
   macOS, `%APPDATA%\Bookclerk\bookclerk\config` on Windows) holds a format
   version, `mode = "branch"` and the patron's own portal session token.
   Nothing assumes that patron is an Owner.
3. **The profile holds a session, not a user id.** A bare user id could be
   edited into anyone. The session is an ordinary portal session, listed in
   the patron's **Sessions** as **Desktop app on <computer>** and revocable
   there. The file is written owner-only (`0600` in a `0700` directory on
   Unix), never logged, and never on argv. The window swaps it for a fresh
   session, lasting 90 days, on every launch.
4. **Operator only until there is an Owner.** While the library has no
   active Owner (first run, or a computer that is only a clerk of another
   library) the window acts as the operator, as `bookclerkd`'s sign-in does.
   Creating the first Owner in the window signs the window in as that
   Owner. A clerk has no patrons of its own, so it needs no profile: its
   library card is its identity.
5. **The host keeps the window's cookies.** The embedded host holds the
   patron's session cookie, and any operator session an elevation sets, in a
   jar the page never sees, and adds them to each request in place of
   anything the page sent. Elevation, impersonation and **Stop** use the
   routes the web app uses, unchanged. The window refuses other origins
   before routing, so the cookie CSRF check does not apply to its requests.
6. **The profile follows sign-in and sign-out only.** A response that sets
   the patron's session while no elevation is active (a sign-in on the
   window's sign-in screen) writes the profile; signing out deletes it, and
   the window returns to the sign-in screen. Elevating, impersonating and
   ending either never touch it.
7. **Several people use several OS accounts.** The window has no patron
   switcher of its own. To share one library on a computer, one account's
   install is the branch (or runs `bookclerkd` as a service) and the others
   sign in to it with library cards.

## Consequences

- The operator is one elevation away in the window, with a password (a
  passkey cannot be the elevation step until the window has a real origin
  for WebAuthn). An OS prompt (polkit, Touch ID, Windows Hello) could be
  offered later, but it proves only the same OS account that already holds
  the files directory.
- Anything that runs as the same OS account can read the profile, as it can
  already read `master.key` and the operator token in the files directory.
  The profile adds no reach the account did not have.
- A computer left unopened for 90 days opens signed out.
- Per-person features in the window (listening progress first) resolve the
  person through the normal caller path instead of a desktop special case.
- The parser is an untrusted-input boundary with the `desktop_profile` fuzz
  target ([fuzzing](../contributing/fuzzing.md)).

Supersedes the "local user = owner" goal recorded for the desktop window
spike. See also [first-party identity](first-party-identity.md) and the
[window section of the UI guide](../contributing/ui/README.md#the-desktop-window).
