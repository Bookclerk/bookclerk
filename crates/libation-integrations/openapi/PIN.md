# Audiobookshelf OpenAPI pin

- Spec file: `audiobookshelf.openapi.json` (from ABS `docs/openapi.json`)
- ABS release family: **v2.35.1** (fetched from `master` at pin time; image tag in CI should match)
- Upstream: https://github.com/advplyr/audiobookshelf

## Coverage note

The committed ABS OpenAPI is auto-generated and **does not yet document** several
routes we rely on (`POST /login`, `POST /api/authorize`, `POST /api/libraries/{id}/scan`,
`GET|POST /api/users`, Socket.io `user_added`, etc.). For those endpoints,
`AbsApiClient` follows `server/routers/ApiRouter.js` and controllers at the same
ABS revision. When OpenAPI gains those paths, prefer aligning types to the spec.
