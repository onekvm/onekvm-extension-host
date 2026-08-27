use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::Value;

use crate::manifest::{Installed, SettingProperty, SettingsSchema};
use crate::paths;
use crate::state::{prepare_runtime_dirs, PluginState};
use crate::sys;

pub fn settings_for_state(extension: &Installed, state: &PluginState) -> Result<Value, String> {
    let mut settings = default_settings(&extension.manifest.settings_schema)?;
    if let Value::Object(map) = &mut settings {
        for (key, value) in &state.settings {
            if extension
                .manifest
                .settings_schema
                .properties
                .get(key)
                .map(|property| property.read_only)
                .unwrap_or(false)
            {
                continue;
            }
            map.insert(key.clone(), value.clone());
        }
    }
    validate_settings(&extension.manifest.settings_schema, &settings)?;
    Ok(settings)
}

fn default_settings(schema: &SettingsSchema) -> Result<Value, String> {
    let mut settings = serde_json::Map::new();
    for (name, property) in &schema.properties {
        if let Some(default) = &property.default {
            settings.insert(name.clone(), default.clone());
        }
    }
    Ok(Value::Object(settings))
}

fn validate_settings(schema: &SettingsSchema, settings: &Value) -> Result<(), String> {
    let Value::Object(map) = settings else {
        return Err("settings must be an object".into());
    };
    for name in map.keys() {
        if !schema.properties.contains_key(name) {
            return Err(format!("unknown setting {name:?}"));
        }
    }
    for name in &schema.required {
        if !map.contains_key(name) {
            return Err(format!("required setting {name:?} is missing"));
        }
    }
    for (name, value) in map {
        validate_value(&schema.properties[name], value)
            .map_err(|err| format!("setting {name}: {err}"))?;
    }
    for (name, property) in &schema.properties {
        if !condition_matches(property.required_when.as_ref(), map) {
            continue;
        }
        match map.get(name) {
            Some(Value::String(text)) if text.is_empty() => {
                return Err(format!(
                    "setting {name} is required by {}",
                    property.required_when.as_ref().unwrap().field
                ));
            }
            None => {
                return Err(format!(
                    "setting {name} is required by {}",
                    property.required_when.as_ref().unwrap().field
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn condition_matches(
    condition: Option<&crate::manifest::SettingCondition>,
    settings: &serde_json::Map<String, Value>,
) -> bool {
    let Some(condition) = condition else {
        return false;
    };
    settings.get(&condition.field) == Some(&condition.equals)
}

fn validate_value(property: &SettingProperty, value: &Value) -> Result<(), String> {
    match property.type_name.as_str() {
        "string" => {
            let Value::String(text) = value else {
                return Err("must be a string".into());
            };
            if property.max_length.is_some_and(|limit| text.len() > limit) {
                return Err(format!(
                    "exceeds maxLength {}",
                    property.max_length.unwrap()
                ));
            }
        }
        "integer" | "number" => {
            let Some(number) = value.as_f64() else {
                return Err("must be a number".into());
            };
            if property.type_name == "integer" && number.fract() != 0.0 {
                return Err("must be an integer".into());
            }
            if property.minimum.is_some_and(|min| number < min) {
                return Err(format!("must be at least {}", property.minimum.unwrap()));
            }
            if property.maximum.is_some_and(|max| number > max) {
                return Err(format!("must be at most {}", property.maximum.unwrap()));
            }
        }
        "boolean" => {
            if !value.is_boolean() {
                return Err("must be a boolean".into());
            }
        }
        other => return Err(format!("has unsupported type {other:?}")),
    }
    if !property.enum_values.is_empty() && !property.enum_values.iter().any(|candidate| candidate == value)
    {
        return Err("must be one of the declared enum values".into());
    }
    Ok(())
}

pub fn write_runtime_settings(id: &str, settings: &Value) -> Result<(), String> {
    prepare_runtime_dirs(id)?;
    let mut data = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    data.push(b'\n');
    let destination = Path::new(paths::RUNTIME_ROOT).join(id).join("config.json");
    if fs::read(&destination).ok().as_deref() == Some(data.as_slice()) {
        return Ok(());
    }
    sys::atomic_write(&destination, &data, 0o600)?;
    let (uid, gid) = sys::runtime_ids()?;
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| "path contains NUL".to_string())?;
    if unsafe { libc::chown(c_path.as_ptr(), uid, gid) } != 0 {
        return Err("chown runtime settings".into());
    }
    Ok(())
}

pub fn repair_settings_output_permissions() -> Result<(), String> {
    let entries = match fs::read_dir(paths::APP_DATA_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) && paths::valid_id(&id) {
            repair_settings_output_permission(&id)?;
        }
    }
    Ok(())
}

pub fn apply_update(
    extension: &Installed,
    current: &Value,
    update: &serde_json::Map<String, Value>,
) -> Result<Value, String> {
    let mut result = match current {
        Value::Object(map) => map.clone(),
        _ => return Err("settings must be an object".into()),
    };
    for (key, value) in update {
        let property = extension
            .manifest
            .settings_schema
            .properties
            .get(key)
            .ok_or_else(|| format!("unknown setting {key:?}"))?;
        if property.read_only {
            return Err(format!("setting {key:?} is read-only"));
        }
        result.insert(key.clone(), value.clone());
    }
    let settings = Value::Object(result);
    validate_settings(&extension.manifest.settings_schema, &settings)?;
    Ok(settings)
}

pub fn redact(manifest: &crate::manifest::Manifest, settings: &Value) -> (Value, Vec<String>) {
    let mut result = settings.clone();
    let mut configured = Vec::new();
    if let Value::Object(map) = &mut result {
        for pointer in &manifest.secret_fields {
            let name = pointer.trim_start_matches('/');
            if let Some(Value::String(secret)) = map.get(name) {
                if !secret.is_empty() {
                    configured.push(pointer.clone());
                }
            }
            map.remove(name);
        }
    }
    configured.sort();
    (result, configured)
}

pub fn merge_output(extension: &Installed, settings: &mut Value) -> Result<(), String> {
    let path = Path::new(paths::APP_DATA_ROOT)
        .join(&extension.manifest.id)
        .join(paths::SETTINGS_OUTPUT);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => return Err(format!("read extension settings output: {err}")),
    };
    if data.len() > 64 << 10 {
        return Err("extension settings output exceeds 64 KiB".into());
    }
    let output: serde_json::Map<String, Value> = serde_json::from_slice(&data)
        .map_err(|e| format!("decode extension settings output: {e}"))?;
    let Value::Object(map) = settings else {
        return Err("settings must be an object".into());
    };
    for (name, value) in output {
        let property = extension
            .manifest
            .settings_schema
            .properties
            .get(&name)
            .ok_or_else(|| format!("extension settings output contains unknown setting {name:?}"))?;
        if !property.read_only {
            return Err(format!(
                "extension settings output contains writable setting {name:?}"
            ));
        }
        validate_value(property, &value)
            .map_err(|err| format!("extension settings output {name}: {err}"))?;
        map.insert(name, value);
    }
    validate_settings(&extension.manifest.settings_schema, settings)
        .map_err(|err| format!("validate extension settings output: {err}"))
}

pub fn service_enabled(settings: &Value) -> bool {
    match settings.get("service_enabled") {
        None => true,
        Some(Value::Bool(value)) => *value,
        _ => true,
    }
}

pub fn repair_settings_output_permission(id: &str) -> Result<(), String> {
    let path = Path::new(paths::APP_DATA_ROOT).join(id).join(paths::SETTINGS_OUTPUT);
    let info = match fs::symlink_metadata(&path) {
        Ok(info) => info,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    if !info.is_file() {
        return Err(format!("extension settings output for {id} is not a regular file"));
    }
    let (_, gid) = sys::runtime_ids()?;
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "path contains NUL".to_string())?;
    if unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) } != 0 {
        return Err(format!("set extension settings output group for {id}"));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).map_err(|e| {
        format!("set extension settings output mode for {id}: {e}")
    })
}
