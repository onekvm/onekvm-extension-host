use dbus::blocking::Connection;
use dbus::message::MatchRule;
use dbus_crossroads::{Crossroads, MethodErr};
use serde::Serialize;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const BUS_NAME: &str = "org.onekvm.ExtensionHost1";
const OBJECT_PATH: &str = "/org/onekvm/ExtensionHost1";
const INTERFACE: &str = "org.onekvm.ExtensionHost1";
const CORE_BUS_NAME: &str = "org.onekvm.Core1";
const EXTENSIONS_SLICE: &str = "system-onekvm-extensions.slice";
const DEFAULT_MANAGER: &str = "/usr/bin/onekvm-plugin-manager";

#[derive(Default)]
struct HostState {
    core_generation: String,
}

#[derive(Serialize)]
struct DispatchRequest<'a> {
    method: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    core_generation: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    id: &'a str,
    #[serde(skip_serializing_if = "is_zero")]
    revision: u64,
    #[serde(skip_serializing_if = "str::is_empty")]
    settings_json: &'a str,
    changed_keys: &'a [String],
    #[serde(skip_serializing_if = "str::is_empty")]
    extension_method: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    request_json: &'a str,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn request<'a>(method: &'a str, generation: &'a str) -> DispatchRequest<'a> {
    DispatchRequest {
        method,
        core_generation: generation,
        id: "",
        revision: 0,
        settings_json: "",
        changed_keys: &[],
        extension_method: "",
        request_json: "",
    }
}

fn dispatch(request: &DispatchRequest<'_>) -> Result<String, MethodErr> {
    let payload = serde_json::to_vec(request)
        .map_err(|error| MethodErr::failed(&format!("encode host request: {error}")))?;
    let manager =
        std::env::var("ONEKVM_PLUGIN_MANAGER").unwrap_or_else(|_| DEFAULT_MANAGER.to_owned());
    let mut child = Command::new(&manager)
        .arg("host-dispatch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| MethodErr::failed(&format!("start {manager}: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| MethodErr::failed("host dispatcher stdin is unavailable"))?
        .write_all(&payload)
        .map_err(|error| MethodErr::failed(&format!("write host request: {error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| MethodErr::failed(&format!("wait for host dispatcher: {error}")))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(MethodErr::failed(if message.is_empty() {
            "extension host dispatcher failed"
        } else {
            &message
        }));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|error| MethodErr::failed(&format!("host response is not UTF-8: {error}")))
}

fn stop_extension_slice() {
    let _ = Command::new("/bin/systemctl")
        .args(["stop", EXTENSIONS_SLICE])
        .status();
}

fn install_signal_handler() -> Result<(), Box<dyn std::error::Error>> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    std::thread::Builder::new()
        .name("signal".to_owned())
        .spawn(move || {
            if signals.forever().next().is_some() {
                stop_extension_slice();
                std::process::exit(0);
            }
        })?;
    Ok(())
}

fn run_host() -> Result<(), Box<dyn std::error::Error>> {
    install_signal_handler()?;
    let connection = Connection::new_system()?;
    connection.request_name(BUS_NAME, false, true, false)?;

    let state = Arc::new(Mutex::new(HostState::default()));
    let interface_state = Arc::clone(&state);
    let mut crossroads = Crossroads::new();
    let interface = crossroads.register(INTERFACE, move |builder| {
        let apply_state = Arc::clone(&interface_state);
        builder.method(
            "ApplyConfig",
            ("id", "revision", "settings_json", "changed_keys"),
            (),
            move |_,
                  _,
                  (id, revision, settings_json, changed_keys): (
                String,
                u64,
                String,
                Vec<String>,
            )| {
                let state = apply_state
                    .lock()
                    .map_err(|_| MethodErr::failed("host state lock is poisoned"))?;
                let mut request = request("ApplyConfig", &state.core_generation);
                request.id = &id;
                request.revision = revision;
                request.settings_json = &settings_json;
                request.changed_keys = &changed_keys;
                dispatch(&request)?;
                Ok(())
            },
        );
        for (name, method) in [
            ("Enable", "Enable"),
            ("Disable", "Disable"),
            ("Reconcile", "Reconcile"),
        ] {
            let action_state = Arc::clone(&interface_state);
            builder.method(
                name,
                ("id", "revision"),
                (),
                move |_, _, (id, revision): (String, u64)| {
                    let state = action_state
                        .lock()
                        .map_err(|_| MethodErr::failed("host state lock is poisoned"))?;
                    let mut request = request(method, &state.core_generation);
                    request.id = &id;
                    request.revision = revision;
                    dispatch(&request)?;
                    Ok(())
                },
            );
        }
        let reconcile_state = Arc::clone(&interface_state);
        builder.method(
            "ReconcileAll",
            ("core_generation",),
            (),
            move |_, _, (core_generation,): (String,)| {
                let mut state = reconcile_state
                    .lock()
                    .map_err(|_| MethodErr::failed("host state lock is poisoned"))?;
                state.core_generation.clone_from(&core_generation);
                let request = request("ReconcileAll", &state.core_generation);
                dispatch(&request)?;
                Ok(())
            },
        );
        let invoke_state = Arc::clone(&interface_state);
        builder.method(
            "Invoke",
            ("id", "method", "request_json"),
            ("response_json",),
            move |_, _, (id, method, request_json): (String, String, String)| {
                let state = invoke_state
                    .lock()
                    .map_err(|_| MethodErr::failed("host state lock is poisoned"))?;
                let mut request = request("Invoke", &state.core_generation);
                request.id = &id;
                request.extension_method = &method;
                request.request_json = &request_json;
                Ok((dispatch(&request)?,))
            },
        );
    });
    crossroads.insert(OBJECT_PATH, &[interface], ());

    let state_for_signal = Arc::clone(&state);
    let mut rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
    rule.path = Some("/org/freedesktop/DBus".into());
    connection.add_match(
        rule,
        move |(name, _, new_owner): (String, String, String), _, _| {
            if name == CORE_BUS_NAME && new_owner.is_empty() {
                if let Ok(mut state) = state_for_signal.lock() {
                    state.core_generation.clear();
                }
                stop_extension_slice();
            }
            true
        },
    )?;

    crossroads.serve(&connection)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() == Some("--plugin-helper") {
        return helper::run(arguments.collect());
    }
    run_host()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_omits_empty_optional_fields() {
        let encoded = serde_json::to_string(&request("ReconcileAll", "generation-1")).unwrap();
        assert_eq!(
            encoded,
            r#"{"method":"ReconcileAll","core_generation":"generation-1","changed_keys":[]}"#
        );
    }
}
mod helper;
