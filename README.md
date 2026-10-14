# onekvm-extension-host

English | [简体中文](README.zh-CN.md)

A low-memory D-Bus process host for OneKVM extensions. The same binary is the
extension manager CLI: no arguments start `org.onekvm.ExtensionHost1`, while
`catalog` / `install` / `enable` run in-process. The CLI name is
`onekvm-extension` (symlink to this binary). Core calls
`/usr/bin/onekvm-extension-host`. `onekvm-extension-manager` remains as a
compatibility symlink.

## Build and test

```sh
cargo build --release --locked
cargo test --locked
```

The component-owned OpenEmbedded recipe lives in `oe-recipes`.
It also installs the systemd and D-Bus policy files. The old
`onekvm-plugin-manager` package is replaced and provided for upgrade
compatibility; its command name remains a symlink to this Rust binary.

## Direct methods and dynamic directories

An API v2 manifest may declare `methods` as an array. A method can provide a
package-relative `command` and typed `args`; arguments marked `"secret": true`
must be strings and are validated from the JSON request without being appended
to the process argument list. The complete request is provided to the command
on standard input. A command-only extension does not need a resident
entrypoint or health check. Command methods may also be combined with a
legacy `entrypoint` daemon; do not declare `controller` at the same time.

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

## License

MIT. See [LICENSE](LICENSE).
