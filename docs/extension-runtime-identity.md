# Dedicated extension runtime accounts

An API v2 extension can declare `"runtime_identity": "dedicated"` in its
manifest. Add `onekvm-extension-dedicated-account` to the IPK's dependencies.
The installed host provides this capability. A host without it cannot
satisfy the dependency. Omitting the declaration uses the shared account.

The host assigns a system user and private primary group to the extension ID.
They are reserved across upgrades, remove, purge, and reinstall. The same
identity applies to the entrypoint, controller, child services, command
methods, and health checks, including packages with a runtime image. The
existing permissions and broker capabilities still govern device and API
access. Host-managed unprivileged runtimes can opt in.

Keep persistent state under `ONEKVM_EXTENSION_DATA`. Put local control
sockets under `ONEKVM_EXTENSION_SOCKET` and read host-generated settings
from `ONEKVM_EXTENSION_CONFIG/config.json`. On an identity change the host
stops the old runtime before transferring plugin-owned state and recreating
the settings view. File contents and the saved settings are preserved.
Paths outside these directories, mounted descendants, and files owned by
the host stay outside the transfer. Store durable files without hard links.

To roll back to a release using the shared account, install or select that
release with a dedicated-account-capable host still installed. Activate it
with that host before downgrading the host itself. The host migrates the
data back to the shared account when it next prepares the rollback runtime.
