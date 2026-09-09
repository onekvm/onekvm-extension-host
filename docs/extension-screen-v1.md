# OneKVM Screen Alert Protocol v1

Core owns `org.onekvm.Screen` so login and other short notices can appear on
whatever display the machine has: NanoKVM OLED today, a touchscreen later.

```
Bus:        org.onekvm.Screen
Path:       /org/onekvm/Screen
Interface:  org.onekvm.Screen
```

| Method | Signature | Meaning |
| --- | --- | --- |
| `ShowAlert` | `s` → `s` | `alert_json` in, alert id out |
| `Dismiss` | `s` → | alert id; empty string dismisses the current alert |

| Signal | Args | Meaning |
| --- | --- | --- |
| `Alert` | `s id`, `s alert_json` | A display backend should show the alert |
| `Dismissed` | `s id` | A display backend should remove it |

`alert_json`:

```json
{"title":"OneKVM","body":"Press BOOT to confirm login","ttl_ms":120000}
```

Title is at most 32 characters, body 64. `ttl_ms` 0 means until `Dismiss`.
Callers must be a OneKVM extension (cgroup attribution, same as Logging).

Display backends subscribe to the signals and render. They do not own
`org.onekvm.Screen`. OLED writes `/dev/onekvm-oled` SHM; a touchscreen would
draw its own overlay.
