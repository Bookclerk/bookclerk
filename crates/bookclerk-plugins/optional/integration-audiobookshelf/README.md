# Audiobookshelf integration

Outbound Audiobookshelf plugin (scan, listening sync, credential return-visit).

## OpenID Connect (Bookclerk as IdP)

Bookclerk hosts a core OIDC authorization server (`/.well-known/openid-configuration`,
`/oidc/*`) so ABS can use Bookclerk Users as the login source. Configure ABS
OpenID with:

- Issuer: Bookclerk `integrations.public_origin` (or the daemon base URL)
- Client id: `audiobookshelf` (public PKCE client; register redirects such as
  `http://127.0.0.1:13378/auth/openid/callback`)
- Auth method: none / PKCE S256

Consent and token minting require a **User** portal session. The shared operator
token cannot obtain user access tokens. Details: [docs/integrations.md](../../../../docs/integrations.md).
