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

Command-only extensions omit `entrypoint`, `controller`, and `healthcheck`.
The host runs each command in the extension sandbox and tears the helper down
after the request, so no extension process remains resident.

The legacy object form of `methods` remains valid for controller extensions.
