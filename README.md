# onekvm-extension-host

English | [简体中文](README.zh-CN.md)

A low-memory D-Bus process host for OneKVM extensions. The same binary is the
extension manager CLI: no arguments start `org.onekvm.ExtensionHost1`, while
`catalog` / `install` / `enable` run in-process. The CLI name is
`onekvm-extension` (symlink to this binary). Core calls
`/usr/bin/onekvm-extension-host`. `onekvm-extension-manager` remains as a
compatibility symlink.

Environment packages (`kind: environment`) are ready after installation. They
have no enable switch or resident service. Their declared kernel modules are
dependencies, not a preload list: Network settings loads only the modules needed
by configured interfaces. Upgrading an older network environment clears its
enabled state while preserving its settings; already loaded modules are left
alone. The host also skips preloading during startup and staged update restore.
When an interface is configured, Core requests a single allowlisted module via
the root-only `LoadNetworkModule(st)` D-Bus method (reserved revision is zero),
exposed by `onekvm-extension load-network-module <module>`. This keeps
`CAP_SYS_MODULE` out of Core. The boot-time CLI can load modules directly before
Core and the extension host start. Disabled VPN clients request no modules.

## Kernel compatibility

Kernel module packages must carry `X-OneKVM-Kernel-Release` and
`X-OneKVM-Kernel-Magic` matching the signed image's
`/usr/share/onekvm/kernel.json`. All downloaded and local dependencies are
checked before installation or stopping an existing extension. Firmware
preparation also compares magic, so a same-release kernel rebuild triggers
the module dependency checks. Legacy images without identity metadata cannot
install additional modules; target firmware must provide kernel metadata.

## Marketplace package sizes

Marketplace download sizes include the compatible package candidate and its
recursive `Pre-Depends`, `Depends`, and `Recommends`, matching the default opkg
installation policy. Version constraints, alternatives and virtual providers are
resolved against the configured architecture priorities. Shared packages are
counted once, and installed packages satisfying a requirement add no download
or storage cost. Required upgrades are included. Installed size uses the feed's
unpacked payload sizes; the plugin's own footprint remains visible when installed.
The cache tracks opkg status as well as feed and architecture configuration.

## Build and test

```sh
cargo build --release --locked
cargo test --locked
```

The component-owned OpenEmbedded recipe lives in `oe-recipes`.
It also installs the systemd and D-Bus policy files. The old
`onekvm-plugin-manager` package is replaced and provided for upgrade
compatibility; its command name remains a symlink to this Rust binary.

## Configuration payloads

`configure <id>` reads a complete JSON request from stdin. Screen media is
passed through without a file size or duration quota, so base64 payloads are
not truncated by the extension host. Plugin settings schemas still validate
the fields and formats they declare.

System plugins may expose an explicit `dbus.methods` map from public method
names to D-Bus members. The generic `invoke` CLI forwards only declared methods,
using a JSON string request and response. It does not permit arbitrary members.

## Direct methods and dynamic directories

The `detail <id>` command (also exposed through Core's `GET /api/extensions/{id}`)
includes declared `permissions` and `kernel_modules`. `package_dependencies`
contains the owning installed package's direct `Depends` entries, preserving
version constraints; it is `null` when package metadata is unavailable or belongs
to a different active plugin version, and an empty array when no dependencies
are declared. Plugin payloads persist across A/B OS slot switches. If the current
slot has no matching package record, dependencies are read from another slot's
record only when its extension version matches the active plugin. Validated
records are retained per package and extension version under
`/data/pkg-state/extensions`, so subsequent OS updates preserve the dependency
information. The current slot takes precedence, including records with no
dependencies. For available plugins the entries
come from the feed's package metadata. System plugin package ownership is resolved
from the opkg file list for its registration.

An API v2 manifest may declare `methods` as an array. A method can provide a
package-relative `command` and typed `args`; arguments marked `"secret": true`
must be strings and are validated from the JSON request without being appended
to the process argument list. The complete request is provided to the command
on standard input. A command-only extension does not need a resident
entrypoint or health check. Command methods may also be combined with a
legacy `entrypoint` daemon; do not declare `controller` at the same time.

The privileged D-Bus host loads manifest `kernel_modules` during enable and
reconcile, including for command-only environment plugins. The manager CLI
never loads modules itself, so callers such as Core need no `CAP_SYS_MODULE`.

An enabled extension can publish runtime-created directories by atomically
writing `$ONEKVM_EXTENSION_DATA/directories-output.json`:

```json
{
  "directories": [{
    "id": "gateway-0011223344556677",
    "name": "Lab NAS",
    "kind": "virtual-media",
    "role": "gateway",
    "path": "gateways/gateway-0011223344556677",
    "location": "data",
    "writable": false
  }]
}
```

Dynamic entries are merged with the manifest's static `directories`. They are
visible only while the extension is enabled, must use safe relative paths and
`location: "data"`, may not repeat a static ID, and are limited to 32 entries
and a 64 KiB output file. Use an atomic rename so catalog readers never observe
partially written JSON. The optional `name` is the user-facing directory label
(up to 255 UTF-8 bytes, without control characters); `id` remains the stable identity.

## Local extension identity

Core verifies extension connections using Unix `SO_PEERCRED` and the systemd
service cgroup. The host registers the exact service unit and manifest
capabilities in `/run/onekvm/extension-auth/<id>.json`. It no longer generates
bearer tokens or uses `LoadCredential`, so starting an extension does not add a
credential mount. Service sandboxing retains `ProtectControlGroups=yes`.

Older binaries receive `CREDENTIALS_DIRECTORY=/usr/share/onekvm/extension-compat`
with a shared public all-zero `onekvm-token` placeholder. This is an ordinary
file, not an authorization credential. New SDK clients only need their extension
ID. Upgrade matching Core/host packages and the Unix-relay cloud extension
together.

## Socket activation

Entrypoint extensions with `network` permission can declare:

```json
{
  "socket_activation": {
    "address_setting": "listen_address",
    "port_setting": "port"
  }
}
```

The named settings must be a numeric IPv4/IPv6 address string and an integer
TCP port (1–65535). The host starts `onekvm-extension-<id>.socket` with
`Accept=no` and defines the matching sandboxed service as an auxiliary
transient unit. It prepares configuration and Core authorization before
listening. The socket and service follow Core's lifetime; disable, upgrade,
and configuration changes stop the listener before the service. An active
listener counts as a ready extension in the catalog even while its process
is absent. Controllers and `settings_reload: "watch"` cannot be combined
with this declaration.

The executable must consume `LISTEN_PID`/`LISTEN_FDS` (one listener at fd 3)
and should exit successfully after its own idle timeout. The host preserves
`Restart=on-failure`; successful idle exit returns activation to systemd.
No extension code needs permission to create systemd units.

## License

MIT. See [LICENSE](LICENSE).

Lifecycle transaction, recovery, generic contribution notifications and device
acceptance are described in [extension-lifecycle.md](docs/extension-lifecycle.md).

Extension installs (feed downloads and local IPK/IPKS uploads) protect non-extension
packages from `/usr/share/onekvm/opkg-baseline/status`, plus Core, CLI, UI and the
extension host. The installed versions can satisfy dependencies, but extension
transactions cannot replace image packages or shared libraries. Optional runtimes
outside the image and extension packages can still be installed or updated. A
missing/invalid baseline fails closed; newer system dependencies require a signed
sqfs system update. Dependencies are downloaded using the original signed
indexes and package checksum validation, then every selected payload is checked
against the image baseline. Installation uses only those local files with no
software sources configured, so the solver cannot introduce other system
candidates. Local uploads containing image packages are rejected. Explicit system
updates use the target image's separate offline installation path.
