# Audiobookshelf integration

Outbound Audiobookshelf plugin (scan, listening sync, credential return-visit).

## OpenID Connect (Bookclerk as IdP)

Bookclerk hosts a core OIDC authorization server (`/.well-known/openid-configuration`,
`/oidc/*`) so ABS can use Bookclerk Users as the login source. Configure ABS
OpenID with:

- Issuer: Bookclerk `integrations.public_origin` (or the daemon base URL)
- Client id: `audiobookshelf` (plugin-provided public PKCE client; enable it
  under Settings → Sign-in). Redirect URIs are derived from this plugin’s
  `base_url` plus `/auth/openid/callback` (ABS’s own UI, default port 13378 —
  not Bookclerk’s 8787 listen port) and are not edited by hand.
- Auth method: none / PKCE S256

Consent and token minting require a **User** portal session. The shared operator
token cannot obtain user access tokens. Details: [docs/integrations.md](../../../../docs/integrations.md).
