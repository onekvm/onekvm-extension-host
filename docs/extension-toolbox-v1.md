# Toolbox manifest and package protocol v1

Introduced in host `0.1.0-r32`. `toolbox.items` is optional, declarative metadata
available in catalog summaries and details. It does not run extension UI scripts
while listing tools. Localized second-line `subtitle` metadata requires host
`0.1.0-r35` and UI `0.1.0-r67`. Catalog disk format 6 invalidates earlier cached
summaries. A toolbox item may also request the validated `edge-to-edge` content
layout; omitted layout remains padded for compatibility.

The canonical field, registration and lifecycle contract is
[Extension Toolbox API v1](../../onekvm-ui/docs/extension-toolbox-v1.md).
Manifest parsing rejects unknown nested fields. Rust IPK validation, the OE
`onekvm-extension` class and the example's manifest validator validate the same
limits and declared regular resources. New toolbox packages must depend on
host `>= 0.1.0-r35`, UI `>= 0.1.0-r67`, and Core `>= 0.1.0-r113`.

Tool visibility requires an installed, enabled active version. Method execution
still checks the latest manifest and lifecycle; having a visible or pinned tool
does not authorize a method. Disabling/uninstalling an extension invalidates its
runtime authorization and stops its managed workers. DATA persists across ordinary removal
and upgrades; explicit purge/delete_data removes it.
