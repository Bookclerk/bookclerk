# Health

Liveness.

Back to the [web API reference](README.md).

## `GET /health`

Serves `GET /health` with a static `ok` payload for liveness probes.

**Access:** No auth layer. Routes that need a caller say so in `security`; the handler checks it.

**Credentials:** none

### Responses

| Status | Description | Body |
| --- | --- | --- |
| 200 | The daemon is serving HTTP. | [`HealthResponse`](schemas.md#healthresponse) (`application/json`) |

### Example request

```console
$ curl 'http://127.0.0.1:8787/health'
```

### Example response

Status `200`:

```json
{
  "status": "string"
}
```
