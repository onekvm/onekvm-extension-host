# OneKVM Extension Controller Protocol v1

An extension may declare a short-lived, language-independent controller:

```json
{
  "controller": {
    "runtime": "executable",
    "entrypoint": "bin/example",
    "arguments": ["--controller"],
    "timeout_seconds": 10
  },
  "methods": {
    "off": {"available_when_disabled": true}
  }
}
```

The Extension Host writes one event object to stdin. Events are
`config_changed`, `enable`, `disable`, `reconcile`, and `invoke`. Every event
contains `api_version`, `extension_id`, `revision`, `enabled`, the complete
`settings` object, `reason`, and optionally `changed_keys`, `core_generation`,
`method`, and `request`.

The controller writes one action plan to stdout:

```json
{
  "actions": [
    {"type": "remove", "name": "display"},
    {
      "type": "exec",
      "name": "off",
      "command": ["bin/example", "--off"],
      "timeout_seconds": 8
    }
  ],
  "response": {"display": "off"}
}
```

Supported actions are `ensure`, `remove`, `remove_all`, and `exec`. Commands
must resolve to executable regular files inside the extension root. The Host
starts one sandboxed helper service for the extension; controller, method,
action, and managed-service processes are children of that service. The
service is placed directly in `system-onekvm-extensions.slice`, carries the
extension's aggregate resource limits, and is bound to `onekvm.service`.
Dispatches after helper startup do not create transient systemd units.

Extensions declaring the explicit `host-root` permission run that helper as
`root` instead of `onekvm-extension`. The normal filesystem, device,
namespace, syscall, and resource restrictions remain in force. This permission
is intended only for narrowly scoped administrative extensions whose own
runtime drops child processes to a less privileged identity when configured.
The helper retains `CAP_SETUID` and `CAP_SETGID` for that transition; both are
cleared from a child after it switches to the unprivileged identity, while
`RestrictSUIDSGID` continues to prevent creation of SUID/SGID files.

`host-unrestricted` is a separate, high-risk opt-in for a host administration
surface that must behave exactly like a root login on the device. It runs as
`root` without the extension filesystem, device, network, namespace,
capability, syscall, or resource restrictions. The helper remains tied to the
OneKVM service lifecycle, but its processes otherwise receive the host's normal
root environment. Existing `host-root` extensions remain sandboxed.

The system D-Bus service `org.onekvm.ExtensionHost1` exports:

- `ApplyConfig(id, revision, settings_json, changed_keys)`
- `Enable(id, revision)`
- `Disable(id, revision)`
- `Reconcile(id, revision)`
- `ReconcileAll(core_generation)`
- `Invoke(id, method, request_json) -> response_json`

Core calls `ReconcileAll` only after owning `org.onekvm.Core1` and notifying
systemd that it is ready. Revisions are monotonic and controllers must be
idempotent because reconciliation can be retried.

## Static file routes

An extension that only contains browser assets can ask Core to host them
without starting a long-running HTTP process:

```json
{
  "routes": [
    {"path": "", "backend": "file", "auth": "passthrough"}
  ]
}
```

The `file` backend exposes only the active extension version's `web/`
directory at `/plugins/<extension-id>/`. A request for the route root serves
`web/index.html`; relative assets are served from the same directory. Core
accepts only `GET` and `HEAD`, rejects path traversal and escaping symlinks,
and does not read any other extension files. `file` routes do not accept
`socket` or `port` fields.

## Route authentication

Routes accept three authentication modes:

- `session` is served below `/api/plugins/<extension-id>/` and requires a
  valid OneKVM device session.
- `passthrough` is served below `/plugins/<extension-id>/` and also requires a
  valid OneKVM device session.
- `public` is served below `/plugins/<extension-id>/` without a OneKVM device
  session. It is reserved for narrow external identity-provider callbacks
  that authenticate their own unguessable, expiring, single-use state.

Only the route prefix explicitly marked `public` bypasses device login. Do
not use `public` for settings, credential reads, disconnect actions, static
pages, or long-lived sockets.
