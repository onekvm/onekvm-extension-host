# Extension points (manifest version 1)

An extension can provide an extension point, contribute to one, or both. This is
metadata in the active manifest. The host resolves it from enabled extensions
at query time; disabling, activating another version, or removing an extension
changes the registry without a registration or cleanup hook. System plugins may
provide or contribute to points in their registration JSON as well.

```json
{
  "extension_points": [{ "id": "screen-display.widget", "version": 1 }]
}
```

The point ID is `<provider-id>.<name>`. The provider owns the contract for
`data` and publishes its schema. A contributor declares an item in its own
manifest:

```json
{
  "contributions": [{
    "point": "screen-display.widget",
    "id": "temperature",
    "version": 1,
    "data": { "title": "Temperature", "glyph": "°", "preview": "--°" },
    "method": "screen_widget_state"
  }],
  "methods": [{ "name": "screen_widget_state", "command": ["bin/widget-state"] }]
}
```

`id` is local to the contributor. The registry returns `<contributor-id>/<id>`
as the stable item ID, so two plugins can use the same local name. `version`
must equal the enabled provider's point version. `method` is optional and must
name a declared public method of the contributor; the consumer invokes it
through `onekvm-extension invoke <source> <method>`. Static registration never
executes the contributor. A consumer must validate `data` against its own
contract before use. Registration data is limited to 16 KiB per item.

```sh
onekvm-extension contributions screen-display.widget
```

The command returns a JSON array of `{point,id,source,version,generation,permissions,data,method}`.
`permissions` comes from the resolved source manifest, not contribution data.
`generation` changes with the source lifecycle, including a rapid disable/enable.
Contributions from installed extensions also include `package_root`, the
read-only installation root for resolving provider-defined safe relative
assets. System-plugin contributions omit it. Consumers must validate any
relative path against their own contract before opening it.
It returns an empty array if the point provider is absent or disabled. The
consumer handles refresh, method polling, timeouts, and failures; one
contributor cannot replace another's namespaced item.

For a screen App, the screen provider defines `screen-display.app` version 1.
The contribution data uses a safe package-relative `bundle` directory:

```json
{
  "contributions": [{
    "point": "screen-display.app",
    "id": "main",
    "version": 1,
    "data": {
      "title": "WireGuard",
      "glyph": "W",
      "bundle": "screen-app",
      "controls": ["touch", "rotary"],
      "order": 40
    }
  }]
}
```

The directory must contain `main.vue-vapor.js` and `main.vue-vapor.pak`.
The screen launcher uses the namespaced ID `<source>/<id>` and loads the
bundle from the resolved `package_root`.

The generic change token and transactional installation/removal semantics are
documented in [extension-lifecycle.md](extension-lifecycle.md). Pending upgrade
contributions are withheld until the transaction commits. Consumers must retire
old sessions and resources when a contribution's generation or content changes.
