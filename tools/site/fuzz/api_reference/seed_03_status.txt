# Status

Library and daemon snapshot.

Back to the [web API reference](README.md).

## `GET /api/status`

Returns library counts, listen addresses, storage backends, and whether operator auth is on.

**Access:** The operator, or a signed-in Administrator, Owner or user allowed to acquire.

**Credentials:** `operatorToken` or `operatorSession` or `portalSession`

### Responses

| Status | Description | Body |
| --- | --- | --- |
| 200 | Library counts and daemon state. | [`StatusResponse`](schemas.md#statusresponse) (`application/json`) |
| 401 | The caller is not signed in, or the credential is wrong. | [`ApiError`](schemas.md#apierror) (`application/json`) |
| 403 | The caller's role does not allow this route. | [`ApiError`](schemas.md#apierror) (`application/json`) |
| 500 | The library could not be read. | [`ErrorJson`](schemas.md#errorjson) (`application/json`) |
| 504 | The request ran past the API's time budget. | string (`text/plain`) |

### Example request

```console
$ curl 'http://127.0.0.1:8787/api/status' \
    -H 'Authorization: Bearer <operatorToken>'
```

### Example response

Status `200`:

```json
{
  "accounts": 0,
  "books": 0,
  "acquired": 0,
  "pending": 0,
  "error": 0,
  "in_progress": 0,
  "listen": "string",
  "storage_backend": "string",
  "auth_enabled": true,
  "events": {
    "pending": 0,
    "running": 0,
    "suspended": 0,
    "dead_letter": 0,
    "acked": 0,
    "oldest_pending_age_secs": 0,
    "retries_total": 0,
    "suspensions_total": 0,
    "dead_letters_total": 0,
    "dispatch_latency_ms_avg": 0,
    "handler_latency_ms_avg": 0
  }
}
```

## `GET /api/cluster/status`

`GET /api/cluster/status`: cluster-wide health every node administrator
may see (node counts, committed capacity, active jobs, delivery backlog),
plus detail for the nodes the caller may see.

**Access:** The operator, a signed-in Owner, Administrator or Host, or a user who owns at least one node.

**Credentials:** `operatorToken` or `operatorSession` or `portalSession`

### Responses

| Status | Description | Body |
| --- | --- | --- |
| 200 | Cluster-wide node health. | any JSON (`application/json`) |
| 400 | The body is invalid. | [`ApiError`](schemas.md#apierror) (`application/json`) |
| 401 | The caller is not signed in. | [`ApiError`](schemas.md#apierror) (`application/json`) |
| 403 | The caller may not do this. | [`ApiError`](schemas.md#apierror) (`application/json`) |
| 500 | The configuration could not be read or written. | [`ApiError`](schemas.md#apierror) (`application/json`) |
| 504 | The request ran past the API's time budget. | string (`text/plain`) |

### Example request

```console
$ curl 'http://127.0.0.1:8787/api/cluster/status' \
    -H 'Authorization: Bearer <operatorToken>'
```
