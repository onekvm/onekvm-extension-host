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

Screen alerts use `org.onekvm.Screen` (owned by Core). OLED, a future
touchscreen, or any other display backend can subscribe:

- Bus: `org.onekvm.Screen`
- Path: `/org/onekvm/Screen`
- Interface: `org.onekvm.Screen`
- `ShowAlert(s alert_json) -> s id`
- `Dismiss(s id)`
- Signals: `Alert(s id, s alert_json)`, `Dismissed(s id)`

`alert_json` is `{"title":"...","body":"...","ttl_ms":120000}`. An empty `id`
dismisses the current alert. Display backends render the alert; they do not
own the well-known name.
