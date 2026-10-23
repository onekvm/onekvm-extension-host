# OneKVM Extension Command Method Protocol v1

An extension that does not need a resident process can expose short-lived CLI
commands directly from its manifest:

```json
{
  "methods": [
    {
      "name": "read",
      "command": ["bin/example", "read"],
      "args": [
        {"name": "source", "type": "string", "required": true},
        {"name": "id", "type": "string", "required": true},
        {"name": "password", "type": "string", "required": true, "secret": true}
      ],
      "timeout_seconds": 10
    }
  ]
}
```

`name` follows the public method naming rules. `command` is a non-empty,
extension-relative static argument vector. Each item in `args` declares one
field in the invoke request object. Supported types are `string`, `integer`,
`number`, and `boolean`; required arguments must precede optional arguments.
String arguments can set `"secret": true`. Secret values are validated from
the request but are not appended to the process argument list.

The host rejects missing, unknown, or incorrectly typed request fields, then
appends their values to `command` in declaration order. Optional positional
arguments cannot be skipped when a later optional argument is present.

The CLI receives the original request JSON on stdin as well as the validated
non-secret positional arguments. It must read secret values only from stdin,
exit successfully, and write exactly one JSON value to stdout; that value is
returned by `Invoke`. Diagnostics belong on stderr. Input and output are each
limited to 1 MiB. The default timeout is 10 seconds and an explicit timeout
must be between 1 and 30 seconds.

Failed captured commands return the sandbox's diagnostic through D-Bus and the
CLI, including command stderr, rather than only an `ExecCapture` errno.

## Installation progress

The privileged marketplace `install_progress` response includes `log` (UTF-8
text) and `log_truncated` alongside the existing phase and percentage fields.
The CLI forwards this read-only query without acquiring the installation lock
or running transaction recovery, so polling remains available during installation.
The log includes package-list refresh, download/install stdout and stderr,
validation, activation, and rollback. It retains the most recent 64 KiB and
remains available after completion or failure; starting a new job clears it.
Clients may poll while the job runs and keep the final output visible.

Extensions that only need short-lived commands omit `entrypoint`,
`controller`, and `healthcheck`. The host runs each command in the extension
sandbox and tears the helper down after the request.

## Package source signing keys

For extensions with `opkg-admin`, the privileged host handles `list_sources`,
`set_source`, `set_source_enabled`, `remove_source` and `inspect_source_key`.
The extension must declare each method in its manifest. Signing keys are never
imported by the extension sandbox.

`inspect_source_key` accepts `{ "public_key": "<ASCII-armored OpenPGP public key>" }`
and returns `{ "fingerprint": "<full primary-key fingerprint>" }` without saving
or trusting it. The host accepts one signing-capable, non-expired, non-revoked
public primary key (including its subkeys), up to 64 KiB. Private keys are rejected.

`set_source` accepts `key` (source identifier), `name`, `feeds_text` and optional
`public_key`. Omitting `public_key` preserves the saved key; an empty string
removes it. External URLs require a public key. URLs under the image's trusted
prefixes may use the image keyring. `list_sources` returns `public_key` and
`key_fingerprint` alongside the existing source fields. Editing a disabled
source preserves its disabled state.

Keys are stored as comment headers in the root-owned source configuration.
Only regular files owned by root and not writable by group/others can grant
external-source trust. Pasted headers in `feeds_text` cannot import keys.
Before a signed index update, the host rebuilds `/run/onekvm/opkg-gpg` from
`/etc/opkg/gpg` and the enabled sources' keys. Image keys remain unchanged.
Changing source configuration invalidates cached lists; the next refresh drops
keys no longer used by any enabled source. opkg has a shared keyring: these keys
are package-signing trust anchors, not a per-URL cryptographic binding.

Feed names must be unique and safe as list filenames. `onekvm-metadata` is
reserved for distribution URLs; the distribution's signed expiration metadata
is still required when any distribution feed is enabled. External-only sources
can use ordinary signed opkg indexes, refreshed at least every 15 minutes.

A command-method extension may still declare `entrypoint` to run a legacy
daemon. In that case the host starts and stops the daemon on enable, disable,
and config changes, and `Invoke` uses the command methods. Do not declare
`controller` together with command methods.

The legacy object form of `methods` remains valid for controller extensions.
