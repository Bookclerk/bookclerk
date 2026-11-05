# Run a server

This shelf is for whoever runs Bookclerk for a household: the daemon, its
configuration, the plugins it loads and the HTTP API it serves. To try
Bookclerk on your own computer first, see
[Getting started](../guide/getting-started.md).

## Pages on this shelf

- [Running Bookclerk](operations.md): the daemon, systemd, Docker and
  logs.
- [Configuration](configuration.md): `config.toml`, environment variables
  and plugin settings.
- [Security checklist](security.md): keys, TLS, sign-in and isolation
  before you expose a server.
- [Database](database.md): SQLite, PostgreSQL or Cloudflare D1, and
  encrypted secrets.
- [Stores reference](stores.md): store plugins, formats and settings.
- [Output and destinations reference](output.md): local and S3
  destinations, naming and packaging.
- [Web app and sign-in reference](sign-in.md): roles, sign-in methods,
  routes and screens.
- [Integrations and sign-in](integrations.md): Audiobookshelf, invites and
  identity providers.
- [Managing plugins](plugins.md): install, approve, configure and place
  plugins; isolation modes.
- [Libraries and branches](cluster-hosts.md): join a library, clerks and
  consent.
- [Media worker](media.md): the confined process that handles audio
  files.
- [Backups, updates, import and export](backups.md): native backups,
  schema backups, updating, Libation and PostgreSQL export.
- [Diagnostics](diagnostics.md): opt-in error reports.
- [CLI reference](cli-reference.md): every built-in `bookclerk` command
  and option (generated).
- [Web API](api/README.md): every route the daemon serves, with example
  requests and responses (generated).

## Next steps

- [Use Bookclerk](../guide/README.md): the web app from a reader's side.
- [Build plugins](../plugins/README.md): add a store, a destination or
  an integration.
