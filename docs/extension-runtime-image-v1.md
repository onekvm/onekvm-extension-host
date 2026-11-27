# Extension runtime images

An API v2 extension with a large, read-only runtime can ship one SquashFS image in its normal extension package. Declare the target architecture as usual and add this object to the manifest. The example uses the extension ID `example-runtime-image`.

```json
{
  "runtime_image": {
    "path": "runtime/example-runtime-image.raw",
    "sha256": "<64 lowercase hexadecimal characters>"
  }
}
```

Package the image in the extension version directory as a root-owned regular
file with mode `0444`. The digest covers the complete image. Declare the
`onekvm-extension-runtime-image` package dependency. Distribute the extension
through a signed feed or in a publisher-signed IPKS.

Keep the manifest, web assets, controller, entrypoint, health check and command
method launchers in the control package. Launcher paths are relative to the
extension version directory, as with an ordinary extension. Launchers can
execute runtime programs by their absolute path, for example
`/usr/lib/onekvm/extensions/example-runtime-image/bin/program`. Put bundled
libraries and static data under that same extension's `lib` and `share`
directories. A launcher can set a program-specific search path when invoking it.
Use normal package dependencies for shared system libraries and kernel modules.
Keep settings, user data and logs in the corresponding existing writable
locations.

The image may contain these directories and their parents, and the release
file. The basename of the mounted tree and the suffix of the release file are
the extension ID.

```text
/usr/lib/onekvm/extensions/example-runtime-image/{bin,lib,share}
/usr/lib/extension-release.d/extension-release.example-runtime-image
```

Include at least one of `bin`, `lib`, or `share`. Use root ownership for
every entry, traversable directories, and ordinary files. The host rejects
links, special files, extended attributes, group or world writable entries
and privileged mode bits. The release file declares the host OS ID,
`ARCHITECTURE` using systemd's architecture names, and the compatible
`VERSION_ID` or `SYSEXT_LEVEL`. Use a SquashFS compression codec supported
by the target kernel.

The image is visible at its `/usr` paths in the host-managed plugin service,
controller, health check and command methods. A directly launched shell
command uses the host view. Plugins using a separately declared systemd unit,
an unrestricted or privileged sandbox, or only UI assets use ordinary
packaging.

Use the standard install, enable, stop, remove and purge operations. The host
checks the image during installation and when opening it for a service, and
releases it when its last service stops. A failing installation restores the
previous extension version and its recorded enabled state. To diagnose a
failure, use the install error and the journal for the per-version
`onekvm-extension-image@….service` or `onekvm-extension-image-check@….service`
named in it.
