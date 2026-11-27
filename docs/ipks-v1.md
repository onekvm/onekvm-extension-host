# OneKVM offline extension packages

An IPKS file is a gzip-compressed POSIX tar archive. A release bundle starts
with `bundle.json` and its detached OpenPGP signature `bundle.json.asc`,
followed by one or more IPKs below `packages/`.

```json
{
  "format": "onekvm-ipks",
  "version": 2,
  "root": "onekvm-extension-example",
  "packages": [
    {
      "file": "onekvm-extension-example_1.0.0-r0_arch.ipk",
      "sha256": "...64 lowercase hexadecimal characters..."
    }
  ]
}
```

The publisher signs the exact bytes of `bundle.json` with the configured
ECDSA P-256 feed key and SHA-256, using an ASCII-armored binary-document
signature. The signed manifest includes the root package name and the filename
and SHA-256 of every IPK. The host checks the signature against the keyring
shipped in the image at `/etc/opkg/gpg/pubring.kbx`, then checks the complete
set of package files and hashes. Keys in the upload and keys configured for
online sources cannot authorize offline release bundles. Preview and install
both verify the upload. Installation verifies again before package
transactions or extension service stops.

The root package is a normal OneKVM extension IPK. Private userspace dependency
package names must start with the root package name plus `-` and may only
install files below the same `/data/onekvm/apps/<id>/<version>` directory.

A bundle may also carry ABI-specific kernel support packages named
`onekvm-kernel-module-*` and, where required by a platform transition,
`onekvm-kernel-modules-base`. Multi-module packages such as
`onekvm-kernel-module-ip-tunnels` and `onekvm-kernel-module-wireguard` are
allowed; their payload is restricted to regular `.ko` files, optional
declarative `modules-load.d` entries (single-module packages only), and the
kernel's dependency metadata below `/usr/lib/modules/<release>`. The bundle's
module IPKs must match the running image's kernel release and magic. System
update supplements must match the target image's kernel. The host
rebuilds module dependency metadata after installation and loads declared
modules only when the extension starts. Kernel modules remain separate IPKs;
they are never folded into a userspace runtime IPK or the base image.

Every package must be reachable from the root package's `Depends` closure.
Package maintenance scripts other than the generated kernel `depmod` hooks,
absolute paths, escaping links, unknown
dependencies, duplicate package names, unsafe system payloads, and unreferenced
packages are rejected before `opkg install` runs.

Base ABI dependencies such as libc and libstdc++ remain system packages. They
are referenced by `Depends` but are not replaced by an uploaded bundle.

## Building and sharing a release bundle

`make ONEKVM_MACHINE=<machine> build extension container` in `onekvm-distro`
builds the package and deploys the signed
`tmp/deploy/images/<machine>/onekvm-extension-container.ipks` in the selected
build directory. The recipe selects the IPKs from the current package build
and signs with the same configured key as the feed. It verifies the result
against the feed public key supplied to the image build. Distribute the
`.ipks` file as a whole. The bundle carries the signature and can be verified
offline by devices whose image trusts that feed key. Online index and IPK
verification remain part of opkg.

## Historical and development uploads

For a historical unsigned v1 bundle, obtain a replacement signed v2 bundle.
On a development device, a device administrator can explicitly authorize unsigned
local uploads for the current boot.

```sh
# Run as root on the development device
install -m 0600 /dev/null /run/onekvm/allow-unsigned-extension-packages
onekvm-extension install-local < development.ipks
rm /run/onekvm/allow-unsigned-extension-packages
```

The authorization also covers the Web upload and preview during that time.
It permits legacy v1 bundles and standalone local extension IPKs, with the
usual package and device checks. For a new unsigned development bundle, invoke
`pack-ipks.py --unsigned-development OUTPUT.ipks ROOT_PACKAGE IPK [IPK...]`.
Even on a development device, v2 requires a valid publisher signature. Remove
the authorization after the install, or reboot the device to clear it.
