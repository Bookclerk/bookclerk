# Troubleshooting

Common problems and what to do about them. If none of these fit, the
operator can look at Bookclerk's log in the terminal or system log (see
[Running Bookclerk](../server/operations.md#logging)).

## Signing in

**The sign-in page asks for an operator token.** Nobody has an Owner account
yet. Print the token with `cargo dev-cli -- daemon token` (or
`bookclerk daemon token`), paste it in, and create the Owner (see
[Getting started](getting-started.md#3-create-your-own-account)).

**The operator token is no longer accepted.** Once an Owner exists, the
token is not accepted on the sign-in page. Sign in as the Owner instead. On
the computer that runs Bookclerk, the tray icon's **Copy sign-in link** or
`bookclerk login` still gives the operator a one-time sign-in link.

**A passkey does not work.** Open Bookclerk at `http://localhost:8787`, not
`http://127.0.0.1:8787`. Passkeys need a name, not a number. On a server,
the operator must set the library's public address.

**An invite link says it is invalid or expired.** Invite links work once
and expire. Ask whoever invited you to send a new one from
**Settings → User Management**.

## Stores

**Accounts says "No storefront plugin is deployed on this node".** The
operator has not turned on any store yet: **Settings → Plugins** (see
[Getting started](getting-started.md#4-turn-on-your-stores)).

**The Connect button is greyed out.** You are signed in as the operator. Stores
belong to people, so sign in as yourself to connect them.

**Amazon asks for a code.** Your Amazon account uses two-step verification.
Enter the code on Amazon's page.

**A scan fails with "no accounts configured".** No store account is
connected yet, or every connected account is turned off for scans.

## Downloads

**Nothing downloads.** Downloads happen only when someone chooses **Acquire
pending**, or when **Auto acquire** is on. Check the job list for errors.

**Downloads fail straight away.** Bookclerk handles audio in a separate,
locked-down helper program, `bookclerk-media-worker`, and refuses to handle
audio without it. If you built Bookclerk yourself, make sure that helper
sits next to `bookclerkd` (`cargo dev` does this for you). See
[Media worker](../server/media.md).

**A store plugin will not start.** Plugins run in their own locked-down
process through `bookclerk-jail` and `bookclerk-workerd`, which must also
sit next to `bookclerkd`. See [Plugins](../internals/guest-jail.md#the-guest-jail).

## Branches

**Join a library says the library is not accepting new branches.** On the
library, an Owner or the operator turns on **Let new branches join** in
**Settings → Branches**.

**Joining fails with an insecure-connection error.** A branch only connects
to a library over HTTPS, except on the same computer. The library's
operator sets that up; see
[Libraries and branches](../server/cluster-hosts.md#on-the-library-let-branches-in).
