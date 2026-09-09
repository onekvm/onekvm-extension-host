use std::fs;
use std::io::{self, Read};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::catalog::extension_running;
use crate::host;
use crate::manifest::{installed_versions, load_installed};
use crate::memory;
use crate::paths;
use crate::ports;
use crate::sandbox;
use crate::settings;
use crate::state::{read_state, write_state, PluginState};
use crate::sys;
use crate::system_plugin::load_system_plugins;

#[derive(Debug, Deserialize)]
struct ConfigureRequest {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    settings: Option<serde_json::Map<String, Value>>,
}

pub fn migrate_legacy_layout() -> Result<(), String> {
    let mut ids = std::collections::BTreeSet::new();
    if let Ok(entries) = fs::read_dir(paths::LEGACY_STATE) {
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) || !name.ends_with(".json") {
                continue;
            }
            let id = name.trim_end_matches(".json");
            if paths::valid_id(id) {
                ids.insert(id.to_owned());
            }
        }
    }
    if let Ok(entries) = fs::read_dir(paths::LEGACY_CONFIG) {
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) && paths::valid_id(&id) {
                ids.insert(id);
            }
        }
    }
    for id in ids {
        let destination = paths::state_path(&id);
        if destination.exists() {
            read_state(&id)?;
            remove_legacy(&id)?;
            continue;
        }
        let mut state = read_legacy_state(&id)?;
        if let Some(settings) = read_legacy_settings(&id)? {
            state.settings = settings;
        }
        write_state(&id, &state)?;
        read_state(&id)?;
        remove_legacy(&id)?;
    }
    Ok(())
}

fn read_legacy_state(id: &str) -> Result<PluginState, String> {
    let data = match fs::read(Path::new(paths::LEGACY_STATE).join(format!("{id}.json"))) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(PluginState::default()),
        Err(err) => return Err(err.to_string()),
    };
    serde_json::from_slice(&data).map_err(|e| format!("invalid legacy state for {id}: {e}"))
}

fn read_legacy_settings(
    id: &str,
) -> Result<Option<std::collections::BTreeMap<String, Value>>, String> {
    let data = match fs::read(Path::new(paths::LEGACY_CONFIG).join(id).join("config.json")) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|e| format!("invalid legacy settings for {id}: {e}"))
}

fn remove_legacy(id: &str) -> Result<(), String> {
    for name in [
        Path::new(paths::LEGACY_STATE).join(format!("{id}.json")),
        Path::new(paths::LEGACY_CONFIG).join(id).join("config.json"),
    ] {
        match fs::remove_file(name) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    match fs::remove_dir(Path::new(paths::LEGACY_CONFIG).join(id)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

pub fn list() -> Result<String, String> {
    let mut output = String::from("ID\tACTIVE\tENABLED\tVERSIONS\n");
    let entries = match fs::read_dir(paths::APPS_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || !paths::valid_id(&id) {
            continue;
        }
        let versions = installed_versions(&id)?;
        let state = read_state(&id)?;
        output.push_str(&format!(
            "{id}\t{}\t{}\t{}\n",
            state.active_version,
            state.enabled,
            versions.join(",")
        ));
    }
    Ok(output)
}

pub fn status(id: &str) -> Result<Value, String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    let state = read_state(id)?;
    let controller = if state.active_version.is_empty() {
        false
    } else {
        load_installed(id, &state.active_version)
            .map(|extension| extension.manifest.controller.is_some())
            .unwrap_or(false)
    };
    Ok(serde_json::json!({
        "id": id,
        "active_version": if state.active_version.is_empty() { Value::Null } else { Value::String(state.active_version) },
        "enabled": state.enabled,
        "running": extension_running(id, controller),
    }))
}

pub fn validate(id: &str, version: &str) -> Result<String, String> {
    let extension = load_installed(id, version)?;
    Ok(format!(
        "{} {} valid\n",
        extension.manifest.id, extension.manifest.version
    ))
}

pub fn activate(id: &str, version: &str) -> Result<(), String> {
    sys::with_lock(|| {
        let extension = load_installed(id, version)?;
        let mut state = read_state(id)?;
        let changed = state.active_version != extension.manifest.version;
        if changed {
            let _ = disable_unlocked(id);
        }
        state.active_version = extension.manifest.version;
        if changed {
            state.revision += 1;
        }
        write_state(id, &state)?;
        if changed && state.enabled {
            enable_unlocked(id)?;
        }
        Ok(())
    })
}

pub fn enable(id: &str) -> Result<(), String> {
    sys::with_lock(|| enable_unlocked(id))
}

pub fn disable(id: &str) -> Result<(), String> {
    sys::with_lock(|| disable_unlocked(id))
}

pub fn enable_unlocked(id: &str) -> Result<(), String> {
    let mut state = read_state(id)?;
    if state.active_version.is_empty() {
        let extension = load_installed(id, "")?;
        state.active_version = extension.manifest.version;
    }
    let extension = load_installed(id, &state.active_version)?;
    host::validate_conflicts(id, &extension.manifest)?;
    let was_enabled = state.enabled;
    if !was_enabled {
        memory::validate(id, &extension.manifest)?;
    }
    crate::sandbox::load_kernel_modules(&extension)?;
    state.enabled = true;
    if !was_enabled {
        state.revision += 1;
    }
    write_state(id, &state)?;
    if was_enabled {
        crate::dbus_client::reconcile(id, state.revision)
    } else {
        crate::dbus_client::enable(id, state.revision)
    }
}

pub fn disable_unlocked(id: &str) -> Result<(), String> {
    let mut state = read_state(id)?;
    if !state.enabled {
        return crate::dbus_client::disable(id, state.revision);
    }
    state.enabled = false;
    state.revision += 1;
    write_state(id, &state)?;
    crate::dbus_client::disable(id, state.revision)
}

pub fn start(id: &str) -> Result<(), String> {
    sys::with_lock(|| {
        let state = read_state(id)?;
        if state.active_version.is_empty() {
            return Err(format!("extension {id} has no active version"));
        }
        let extension = load_installed(id, &state.active_version)?;
        crate::sandbox::load_kernel_modules(&extension)?;
        crate::dbus_client::reconcile(id, state.revision)
    })
}

pub fn stop(id: &str) -> Result<(), String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    sys::with_lock(|| {
        let state = read_state(id)?;
        crate::dbus_client::disable(id, state.revision)
    })
}

pub fn restart(id: &str) -> Result<(), String> {
    sys::with_lock(|| {
        let state = read_state(id)?;
        crate::dbus_client::disable(id, state.revision)?;
        if state.enabled {
            crate::dbus_client::reconcile(id, state.revision)?;
        }
        Ok(())
    })
}

pub fn service_action(id: &str, service: &str, action: &str) -> Result<(), String> {
    if !paths::valid_id(id) || !paths::valid_method_name(service) {
        return Err(format!("invalid extension service {id:?}/{service:?}"));
    }
    if !matches!(action, "start" | "stop" | "restart" | "enable" | "disable") {
        return Err(format!("unsupported service action {action:?}"));
    }
    let (registered_service, controller, enabled) = sys::with_lock(|| {
        let state = read_state(id)?;
        if state.active_version.is_empty() {
            return Err(format!("extension {id} is not active"));
        }
        let extension = load_installed(id, &state.active_version)?;
        let registered_service = extension
            .manifest
            .services
            .iter()
            .find(|item| item.id == service)
            .cloned()
            .ok_or_else(|| format!("extension {id} does not register service {service}"))?;
        Ok((
            registered_service,
            extension.manifest.controller.is_some(),
            state.enabled,
        ))
    })?;
    if !registered_service.unit.is_empty() {
        return sandbox::systemctl(&[action, &registered_service.unit]);
    }
    match action {
        "enable" => enable(id),
        "disable" => disable(id),
        "start" => {
            if !enabled {
                enable(id)
            } else {
                start(id)
            }
        }
        "stop" => {
            if !controller {
                stop(id)
            } else {
                crate::helper_client::call(id, "Remove", &[Value::String(service.to_owned())])
                    .map(|_| ())
            }
        }
        "restart" => {
            if !enabled {
                return Err(format!("extension {id} is disabled"));
            }
            if !controller {
                restart(id)
            } else {
                crate::helper_client::call(id, "Restart", &[Value::String(service.to_owned())])
                    .map(|_| ())
            }
        }
        _ => unreachable!(),
    }
}

pub fn configure(id: &str, stdin: &mut dyn Read) -> Result<(), String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    let mut payload = Vec::new();
    stdin
        .take(1 << 20)
        .read_to_end(&mut payload)
        .map_err(|e| format!("decode extension configuration: {e}"))?;
    let request: ConfigureRequest = serde_json::from_slice(&payload)
        .map_err(|e| format!("decode extension configuration: {e}"))?;
    let system_plugins = load_system_plugins()?;
    if let Some(plugin) = system_plugins.get(id) {
        if request.enabled.is_some() {
            return Err(format!("system plugin {id} cannot be enabled or disabled"));
        }
        let Some(settings) = request.settings else {
            return Ok(());
        };
        return plugin.update_settings(settings);
    }
    sys::with_lock(|| {
        let mut state = read_state(id)?;
        let extension = load_installed(id, &state.active_version)?;
        let state_initialized = state.active_version.is_empty();
        if state_initialized {
            state.active_version = extension.manifest.version.clone();
            state.revision += 1;
        }
        let current = settings::settings_for_state(&extension, &state)?;
        let mut changed_keys = Vec::new();
        if let Some(update) = request.settings {
            let previous = current.clone();
            let settings = settings::apply_update(&extension, &current, &update)?;
            if let (Value::Object(prev), Value::Object(next)) = (&previous, &settings) {
                for key in update.keys() {
                    if prev.get(key) != next.get(key) {
                        changed_keys.push(key.clone());
                    }
                }
            }
            changed_keys.sort();
            let check_port = ports::contains_changed_key(&changed_keys, "port")
                || ports::contains_changed_key(&changed_keys, "listen_address")
                || (ports::contains_changed_key(&changed_keys, "service_enabled")
                    && settings::service_enabled(&settings));
            if check_port {
                let allow_current = !ports::contains_changed_key(&changed_keys, "port")
                    && state.enabled
                    && settings::service_enabled(&previous)
                    && extension_running(id, extension.manifest.controller.is_some());
                ports::validate_extension_port(id, &settings, allow_current)?;
            }
            if let Value::Object(map) = settings {
                state.settings = map.into_iter().collect();
            }
        }
        let enabled_changed = request.enabled.is_some_and(|value| value != state.enabled);
        if enabled_changed && request.enabled == Some(true) {
            host::validate_conflicts(id, &extension.manifest)?;
            memory::validate(id, &extension.manifest)?;
        }
        if enabled_changed && request.enabled == Some(true) {
            let settings = settings::settings_for_state(&extension, &state)?;
            if settings::service_enabled(&settings) {
                ports::validate_extension_port(id, &settings, false)?;
            }
        }
        if !state_initialized && !enabled_changed && changed_keys.is_empty() {
            if extension.manifest.controller.is_none()
                && extension.manifest.settings_reload == "watch"
            {
                settings::write_runtime_settings(id, &current)?;
            }
            return Ok(());
        }
        if let Some(enabled) = request.enabled {
            state.enabled = enabled;
        }
        if enabled_changed || !changed_keys.is_empty() {
            state.revision += 1;
        }
        write_state(id, &state)?;
        if !changed_keys.is_empty() {
            if extension.manifest.controller.is_some() {
                let settings = serde_json::to_string(&Value::Object(
                    state.settings.clone().into_iter().collect(),
                ))
                .map_err(|e| e.to_string())?;
                crate::dbus_client::apply_config(id, state.revision, &settings, &changed_keys)?;
            } else if extension.manifest.settings_reload == "watch" {
                let settings = settings::settings_for_state(&extension, &state)?;
                settings::write_runtime_settings(id, &settings)?;
            } else if state.enabled {
                crate::dbus_client::disable(id, state.revision)?;
                crate::sandbox::load_kernel_modules(&extension)?;
                crate::dbus_client::reconcile(id, state.revision)?;
            }
        }
        if enabled_changed {
            if state.enabled {
                crate::sandbox::load_kernel_modules(&extension)?;
                crate::dbus_client::enable(id, state.revision)?;
            } else {
                crate::dbus_client::disable(id, state.revision)?;
            }
        }
        Ok(())
    })
}

pub fn invoke(id: &str, method: &str, request: &[u8]) -> Result<String, String> {
    crate::dbus_client::invoke(id, method, std::str::from_utf8(request).unwrap_or("null"))
}

pub fn restore() -> Result<(), String> {
    crate::sys::with_lock(crate::state::initialize_default_enabled_states)
        .map_err(|err| format!("initialize default-enabled extensions: {err}"))?;
    let entries = match fs::read_dir(paths::CONFIG_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    let mut failures = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) || !name.ends_with(".json") {
            continue;
        }
        let id = name.trim_end_matches(".json");
        if !paths::valid_id(id) {
            continue;
        }
        match read_state(id) {
            Ok(mut state) if state.enabled => {
                let versions = match installed_versions(id) {
                    Ok(versions) => versions,
                    Err(err) => {
                        failures.push(err);
                        continue;
                    }
                };
                if versions.is_empty() {
                    continue;
                }
                if state.active_version.is_empty() {
                    state.active_version = versions.last().cloned().unwrap_or_default();
                    if let Err(err) = write_state(id, &state) {
                        failures.push(err);
                        continue;
                    }
                }
                match load_installed(id, &state.active_version) {
                    Ok(extension) => {
                        if let Err(err) = crate::sandbox::load_kernel_modules(&extension) {
                            failures.push(format!("{id}: {err}"));
                            continue;
                        }
                        if let Err(err) = crate::dbus_client::reconcile(id, state.revision) {
                            failures.push(format!("{id}: {err}"));
                        }
                    }
                    Err(err) => failures.push(format!("{id}: {err}")),
                }
            }
            Ok(_) => {}
            Err(err) => failures.push(err),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("restore failures: {}", failures.join("; ")))
    }
}
