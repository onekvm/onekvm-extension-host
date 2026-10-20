use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{load_installed, Manifest};
use crate::paths;
use crate::sys;

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct PluginState {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_version: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revision: u64,
    #[serde(default)]
    pub lifecycle_revision: u64,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub settings: std::collections::BTreeMap<String, Value>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

pub fn read_state(id: &str) -> Result<PluginState, String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    let data = match fs::read(paths::state_path(id)) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(PluginState::default()),
        Err(err) => return Err(err.to_string()),
    };
    let state: PluginState =
        serde_json::from_slice(&data).map_err(|e| format!("invalid state for {id}: {e}"))?;
    if !state.active_version.is_empty() && !paths::valid_version(&state.active_version) {
        return Err(format!("invalid active version in state for {id}"));
    }
    Ok(state)
}

pub fn write_state(id: &str, state: &PluginState) -> Result<(), String> {
    let previous = read_state(id)?;
    let mut state = state.clone();
    state.lifecycle_revision = state.lifecycle_revision.max(previous.lifecycle_revision);
    if previous.enabled != state.enabled || previous.active_version != state.active_version {
        state.lifecycle_revision = state.lifecycle_revision.saturating_add(1);
    }
    let data = serde_json::to_vec_pretty(&state).map_err(|e| e.to_string())?;
    let mut payload = data;
    payload.push(b'\n');
    sys::atomic_write(&paths::state_path(id), &payload, 0o600)?;
    crate::contributions::changed()
}

pub fn starts_enabled(manifest: &Manifest) -> bool {
    manifest.default_enabled && manifest.kind != "cloud-provider" && !manifest.is_environment()
}

// Installed environments provide tools and modules, without an enabled runtime.
// Preserve configuration when migrating packages that previously used kind=network.
pub fn normalize_environment(manifest: &Manifest, state: &mut PluginState) -> bool {
    if !manifest.is_environment() || !state.enabled {
        return false;
    }
    state.enabled = false;
    state.revision = state.revision.saturating_add(1);
    true
}

pub fn initialize_default_enabled_states() -> Result<(), String> {
    let entries = match fs::read_dir(paths::APPS_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || !paths::valid_id(&id) {
            continue;
        }
        if paths::state_path(&id).exists() {
            let mut state = read_state(&id)?;
            if let Ok(extension) = load_installed(&id, &state.active_version) {
                if normalize_environment(&extension.manifest, &mut state) {
                    write_state(&id, &state)?;
                }
            }
            continue;
        }
        let Ok(extension) = load_installed(&id, "") else {
            continue;
        };
        if !starts_enabled(&extension.manifest) && !extension.manifest.is_environment() {
            continue;
        }
        if crate::host::validate_device_variant(&extension.manifest).is_err() {
            continue;
        }
        write_state(
            &id,
            &PluginState {
                enabled: starts_enabled(&extension.manifest),
                active_version: extension.manifest.version,
                lifecycle_revision: 0,
                revision: 1,
                settings: std::collections::BTreeMap::new(),
            },
        )?;
    }
    disable_conflicting_cloud_provider_states()
}

fn disable_conflicting_cloud_provider_states() -> Result<(), String> {
    let entries = match fs::read_dir(paths::CONFIG_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    let mut enabled = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
            || !paths::valid_id(id)
        {
            continue;
        }
        let state = read_state(id)?;
        if !state.enabled || state.active_version.is_empty() {
            continue;
        }
        let Ok(extension) = load_installed(id, &state.active_version) else {
            continue;
        };
        if extension.manifest.kind == "cloud-provider" {
            enabled.push((id.to_owned(), state));
        }
    }
    if enabled.len() <= 1 {
        return Ok(());
    }
    enabled.sort_by(|left, right| left.0.cmp(&right.0));
    eprintln!(
        "multiple cloud providers were enabled; disabling {} until the user selects one",
        enabled
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (id, mut state) in enabled {
        state.enabled = false;
        state.revision = state.revision.saturating_add(1);
        write_state(&id, &state)?;
    }
    Ok(())
}

pub fn prepare_runtime_dirs(id: &str) -> Result<(), String> {
    let (uid, gid) = sys::runtime_ids()?;
    fs::create_dir_all(paths::RUNTIME_ROOT).map_err(|e| e.to_string())?;
    set_mode_if_needed(Path::new(paths::RUNTIME_ROOT), 0o755)?;
    let euid = unsafe { libc::geteuid() } as u32;
    let directories = [
        (Path::new(paths::APP_DATA_ROOT).join(id), uid),
        (Path::new(paths::RUNTIME_ROOT).join(id), euid),
        (Path::new(paths::RUNTIME_ROOT).join(id).join("sockets"), uid),
    ];
    for (path, owner) in directories {
        fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        ensure_directory_permissions(&path, owner, gid, euid, 0o750)?;
    }
    Ok(())
}

fn set_mode_if_needed(path: &Path, mode: u32) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    if metadata.mode() & 0o7777 == mode {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())
}

fn ensure_directory_permissions(
    path: &Path,
    owner: u32,
    group: u32,
    euid: u32,
    mode: u32,
) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    let current_owner = metadata.uid();
    let current_group = metadata.gid();
    let mode_matches = metadata.mode() & 0o7777 == mode;
    if current_owner == owner && current_group == group && mode_matches {
        return Ok(());
    }

    let path_c = c_path(path)?;
    let took_ownership = !mode_matches && current_owner != euid;
    if took_ownership && unsafe { libc::chown(path_c.as_ptr(), euid, u32::MAX) } != 0 {
        return Err(format!("take ownership of {}", path.display()));
    }
    if !mode_matches {
        if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
            if took_ownership {
                unsafe { libc::chown(path_c.as_ptr(), current_owner, current_group) };
            }
            return Err(err.to_string());
        }
    }
    if (current_owner != owner || current_group != group || took_ownership)
        && unsafe { libc::chown(path_c.as_ptr(), owner, group) } != 0
    {
        return Err(format!("chown {}", path.display()));
    }
    Ok(())
}

fn c_path(path: &Path) -> Result<std::ffi::CString, String> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| "path contains NUL".into())
}

fn authorization_record(manifest: &Manifest) -> Value {
    // A legacy service may also expose methods through a command helper. Keep
    // both identities stable so invoking a method cannot revoke its daemon.
    serde_json::json!({
        "id": manifest.id,
        "unit": paths::unit_name(&manifest.id),
        "units": [paths::unit_name(&manifest.id), paths::helper_unit(&manifest.id)],
        "capabilities": manifest.capabilities,
    })
}

pub fn prepare_authorization(manifest: &Manifest) -> Result<(), String> {
    fs::create_dir_all(paths::AUTH_ROOT).map_err(|e| e.to_string())?;
    let authorization = authorization_record(manifest);
    let mut body = serde_json::to_vec(&authorization).map_err(|e| e.to_string())?;
    body.push(b'\n');
    sys::atomic_write(
        &Path::new(paths::AUTH_ROOT).join(format!("{}.json", manifest.id)),
        &body,
        0o600,
    )?;
    // Retire bearer credentials left by hosts using the previous protocol.
    let _ = fs::remove_file(Path::new(paths::AUTH_ROOT).join(format!("{}.token", manifest.id)));
    Ok(())
}

pub fn remove_authorization(id: &str) {
    let _ = fs::remove_file(Path::new(paths::AUTH_ROOT).join(format!("{id}.token")));
    let _ = fs::remove_file(Path::new(paths::AUTH_ROOT).join(format!("{id}.json")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_keeps_daemon_and_command_helper_identities() {
        let manifest = Manifest {
            id: "cloud".into(),
            capabilities: vec!["auth".into()],
            ..Manifest::default()
        };
        let record = authorization_record(&manifest);
        assert_eq!(
            record["units"],
            serde_json::json!([
                "onekvm-extension-cloud.service",
                "onekvm-extension-cloud@helper.service"
            ])
        );
        assert_eq!(record["capabilities"], serde_json::json!(["auth"]));
    }

    fn temporary_directory(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "onekvm-extension-host-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn matching_directory_permissions_are_a_noop() {
        let path = temporary_directory("permission-noop");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        let before = fs::metadata(&path).unwrap();
        ensure_directory_permissions(
            &path,
            before.uid(),
            before.gid(),
            unsafe { libc::geteuid() } as u32,
            0o750,
        )
        .unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.gid(), before.gid());
        assert_eq!(after.mode() & 0o7777, 0o750);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn owned_directory_mode_is_repaired() {
        let path = temporary_directory("permission-repair");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        ensure_directory_permissions(
            &path,
            metadata.uid(),
            metadata.gid(),
            unsafe { libc::geteuid() } as u32,
            0o750,
        )
        .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o750);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn cloud_providers_never_start_enabled_by_default() {
        let mut manifest = Manifest {
            default_enabled: true,
            kind: "cloud-provider".into(),
            ..Manifest::default()
        };
        assert!(!starts_enabled(&manifest));
        manifest.kind = "service".into();
        assert!(starts_enabled(&manifest));
        manifest.default_enabled = false;
        assert!(!starts_enabled(&manifest));
    }

    #[test]
    fn environment_upgrade_clears_enabled_without_losing_configuration() {
        let manifest = Manifest {
            kind: "environment".into(),
            ..Manifest::default()
        };
        let mut state = PluginState {
            enabled: true,
            active_version: "1.0.0".into(),
            revision: 4,
            lifecycle_revision: 0,
            settings: [("interface".into(), serde_json::json!("wg0"))].into(),
        };
        assert!(normalize_environment(&manifest, &mut state));
        assert!(!state.enabled);
        assert_eq!(state.revision, 5);
        assert_eq!(state.active_version, "1.0.0");
        assert_eq!(state.settings["interface"], "wg0");
        assert!(!normalize_environment(&manifest, &mut state));
        assert_eq!(state.revision, 5);
        assert!(!crate::catalog::extension_running("test", &manifest, true));
    }
}

pub fn write_generation(generation: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(paths::GENERATION_PATH).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    sys::atomic_write(
        Path::new(paths::GENERATION_PATH),
        format!("{generation}\n").as_bytes(),
        0o600,
    )
}

pub fn read_generation() -> String {
    fs::read_to_string(paths::GENERATION_PATH)
        .ok()
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}
