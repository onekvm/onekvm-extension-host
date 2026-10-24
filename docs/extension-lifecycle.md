# Extension lifecycle and recovery

Extensions register contributions in their own active manifest. Consumers never
maintain a list of contributor IDs or run contributor-specific registration
hooks. Registration follows the enabled state; stopping a service can leave its
management UI available.

All state changes publish the atomic generic change token
`/run/onekvm/contributions-generation`. Consumers subscribe to the parent
directory with inotify before initial
discovery, so first creation and atomic replacement are observed. Reconcile on
startup, watcher reattachment and queue overflow; periodic discovery is not
required. Observe service lifecycle notifications when discovery depends on
runtime readiness.
Resolved entries include `generation`, an extension lifecycle revision. It
changes with enable/disable and active-version changes, including a quick
disable/enable between consumer refreshes. Settings updates keep that revision.
Consumers must distinguish failed discovery from a successful empty registry.

Preinstalled plugins declare `default_enabled` in their manifest. The host
initializes that state only for a supported device with no saved plugin state;
an explicit user disable survives subsequent boots and upgrades. Environment
packages stay available without a resident enabled runtime. Compatibility
declarations must include every machine on which a plugin is preinstalled.

Catalog snapshots include live enabled and service readiness state. Core checks
the generic contribution token before reusing its snapshot, including when the
host initializes defaults after Core's initial catalog warmup. Runtime snapshots
also expire on demand (Core: five seconds; CLI disk cache: two seconds), so a
service starting or stopping independently cannot leave a permanent stale
status. This does not add background polling. In-flight queries retain their
original mutation revision, so a concurrent invalidation is not lost.

## Upgrade and activation transactions

The privileged package-manager host must be able to write signed runtime
dependencies to `/usr`, including `/usr/bin`, `/usr/sbin` and libraries.
`ProtectSystem=strict` stays enabled with `/usr` explicitly writable for that
host; individual extension services keep their own read-only system mounts.
Controller health checks must validate required runtime executables as well as
settings. A missing runtime returns configuration exit status 78 from the
service, which suppresses automatic restart until the installation is repaired.

Before stopping a plugin, the manager records the original configuration and
enabled state in `/data/pkg-state/extensions/transactions/<id>/journal.json`.
After quiescing its processes, it snapshots all installed versions, plugin data,
and its own package records. The snapshot is durable before opkg can replace
files. Same-version reinstalls use the same transaction.

The new version is committed after installed-manifest validation, successful
activation, a resident-process check, and the declared bounded `exec`
healthcheck. Health checks run through the plugin sandbox, not as arbitrary root
commands. A disabled plugin is installed without starting it.

On failure, the manager stops the new runtime and restores the previous payload,
plugin data (including failed configuration/database migrations), package
records, active version and enabled state. Snapshot I/O is delegated to the
root-only host when Core's CLI child lacks filesystem access capabilities.
Recovery errors are returned with the retained snapshot location. A durable
commit marker prevents cleanup failures from rolling back a healthy upgrade.
Interrupted transactions are recovered before host startup or the next CLI
operation. An incomplete pre-install snapshot only restores the original state.
The Core HTTP bridge gives package mutations a five-minute deadline so sandbox
health checks and rollback can finish on slower devices. Older Core versions
may return an earlier HTTP timeout; the durable journal remains recoverable.

The transaction covers the plugin and its private userspace packages. Shared
platform dependencies and kernel packages retain their own opkg records and
are not downgraded by plugin rollback; platform rollback belongs to the system
update mechanism. A plugin health check cannot certify external network access
or every device-specific feature.

## Removal

`remove` stops and disables the extension and removes its package, retaining
configuration and plugin data. `purge` additionally deletes configuration,
plugin data, runtime data and remaining installed versions. Consumers retire
removed sessions and clean their own derived resources; stable layout IDs may
be retained as user preferences. Screen-specific behavior is documented in
`onekvm-screen-display/docs/extension-points.md`.

## Acceptance

Run `cargo test --locked`. Build the isolated probe resources with
`sh tests/build-lifecycle-probe.sh`, then build
`onekvm-extension-lifecycle-probe` using OE/kas. Build a second package with
`PV:pn-onekvm-extension-lifecycle-probe = "2.0.0"` and
`LIFECYCLE_PROBE_FAIL_HEALTH:pn-onekvm-extension-lifecycle-probe = "1"` in a
temporary `.oe/` configuration. Neither package is part of the product image.

After reading the device document, run
`sh tests/test-lifecycle-device.sh root@device healthy.ipk unhealthy.ipk`.
It verifies install/reinstall, a failed upgrade with data rollback, registry
enable/disable, data-retaining removal and purge, then removes the fixture.
Its screen bundle includes previously unknown Chinese title glyphs and an icon
for independent screen-host acceptance without rebuilding the host.
