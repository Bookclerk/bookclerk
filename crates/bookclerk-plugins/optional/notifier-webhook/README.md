# Webhook notifier

A `notifier` plugin (id `webhook`) that posts each notification as JSON to a
URL, signed per the [Standard Webhooks](https://www.standardwebhooks.com)
scheme. It runs in `workerd` and only ever posts to the instance's `url`
setting and to the URLs patrons save.

- A **channel** posts to the `url` setting, signed with the
  `signing_secret` secret.
- A **personal** destination posts to the patron's own `url`, signed with
  their own `secret`. Both fields are sealed; the patron sees only the
  URL's host.
- `send` makes one `POST` and never follows a redirect. `2xx` is
  delivered; `429` is retried after `Retry-After`; `408`, `425`, `5xx`
  and network failures are retried; anything else fails.
- `validateDestination` and `health` check the URL and secret without a
  request.
- Nothing is logged: a URL may hold a token, and a message may be
  sensitive.

Setup, headers and the payload are in
[Notifications](../../../../docs/server/notifications.md#the-webhook-notifier).

## Develop

`src/` is the source and `modules/` is what ships; `npm run build`
regenerates `modules/`, and CI fails when the committed copy differs.

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../crates/bookclerk-plugins/optional/notifier-webhook
npm ci
npx bookclerk-plugin types .   # after editing plugin.toml
npm run typecheck
npm run build
npm test
```

See [Dynamic plugins](../../../../docs/plugins/README.md#first-party-plugins).
