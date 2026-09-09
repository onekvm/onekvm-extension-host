# Extension Auth Provider v1

An extension can declare a second-factor provider:

```json
{
  "capabilities": ["auth"],
  "methods": [
    {"name": "confirm", "command": ["bin/confirm"], "timeout_seconds": 120},
    {"name": "cancel", "command": ["bin/cancel"], "timeout_seconds": 5}
  ],
  "auth_provider": {
    "method": "confirm",
    "cancel_method": "cancel",
    "login": "web/login.js",
    "enroll": "web/enroll.js"
  }
}
```

Core sends a JSON challenge on stdin / Invoke request:

```json
{
  "version": 1,
  "action": "login",
  "username": "admin",
  "challenge_id": "hex",
  "cancel_path": "/run/onekvm/mfa/<id>.cancel"
}
```

The method must print one JSON object:

```json
{"response": "", "result": "ok"}
```

`result` is `ok`, `deny`, `timeout`, or `cancelled`. Login verification is
cancelled from the Web UI (`POST /api/auth/mfa/cancel`); Core writes
`cancel_path` and may Invoke `cancel_method`.

OLED messages use `org.onekvm.Core1.Oled`:

- `ShowAlert(s alert_json) -> s id`
- `DismissAlert(s id)`

`alert_json` is `{"title":"...","body":"...","ttl_ms":120000}`. An empty `id`
dismisses the current alert. Only `nanokvm-oled` writes `/dev/onekvm-oled`.
