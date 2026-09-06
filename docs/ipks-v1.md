# OneKVM IPKS bundle format v1

An IPKS file is a gzip-compressed POSIX tar archive containing `bundle.json`
and one or more IPK files below `packages/`.

```json
{
  "format": "onekvm-ipks",
  "version": 1,
  "root": "onekvm-extension-example",
  "packages": [
    {
      "file": "onekvm-extension-example_1.0.0-r0_arch.ipk",
      "sha256": "...64 lowercase hexadecimal characters..."
    }
  ]
}
```

The root package is a normal OneKVM extension IPK. Private userspace dependency
package names must start with the root package name plus `-` and may only
install files below the same `/data/onekvm/apps/<id>/<version>` directory.

A bundle may also carry ABI-specific NanoKVM kernel support packages named
`onekvm-kernel-module-*` and, where required by a platform transition,
`onekvm-kernel-modules-base`. Their payload is restricted to regular `.ko`
files, optional declarative `modules-load.d` entries, and the kernel's
dependency metadata below `/usr/lib/modules/<release>`. The host rebuilds
module dependency metadata after installation and loads declared modules only
when the extension starts. Kernel modules remain separate IPKs; they are never
folded into a userspace runtime IPK or the base image.

Every package must be reachable from the root package's `Depends` closure.
Package maintenance scripts, absolute paths, escaping links, unknown
dependencies, duplicate package names, unsafe system payloads, and unreferenced
packages are rejected before `opkg install` runs.

Base ABI dependencies such as libc and libstdc++ remain system packages. They
are referenced by `Depends` but are not replaced by an uploaded bundle.
