# OneKVM Extension Logging API v1

Extensions can write attributed events to OneKVM's bounded, rotating log over
the system D-Bus. The API is available while `onekvm.service` owns
`org.onekvm.Core1`:

- Bus name: `org.onekvm.Core1`
- Object path: `/org/onekvm/Core1/Logging`
- Interface: `org.onekvm.Core1.Logging`
- Method: `Log(level, message)` with signature `ss`

Supported levels are `trace`, `debug`, `info`, `warn`, and `error`. Messages
must contain 1 to 4096 bytes and must not contain a NUL byte. A call does not
require a manifest capability. Events below the configured OneKVM logging
level are discarded in the same way as Core events.

For example:

```sh
busctl call org.onekvm.Core1 \
  /org/onekvm/Core1/Logging \
  org.onekvm.Core1.Logging Log ss \
  info "display initialized"
```

The extension ID is intentionally not an argument. Core resolves the D-Bus
sender PID and attributes the event from its
`onekvm-extension-<id>.service` or `onekvm-extension-<id>@helper.service`
cgroup below `system-onekvm-extensions.slice`. Calls from outside an extension
service are rejected, so an extension cannot impersonate another extension in
the shared log.

Do not include passwords, tokens, private keys, or other secrets in messages.
Normal stdout and stderr still go to the extension's systemd journal; use this
API for events that should also appear in the OneKVM log page.
