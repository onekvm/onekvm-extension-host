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
    let data = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let mut payload = data;
    payload.push(b'\n');
    sys::atomic_write(&paths::state_path(id), &payload, 0o600)
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
            continue;
        }
        let Ok(extension) = load_installed(&id, "") else {
            continue;
        };
        if !extension.manifest.default_enabled {
            continue;
        }
        if crate::host::validate_device_variant(&extension.manifest).is_err() {
            continue;
        }
        write_state(
            &id,
            &PluginState {
                enabled: true,
                active_version: extension.manifest.version,
                revision: 1,
                settings: std::collections::BTreeMap::new(),
            },
        )?;
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

pub fn prepare_credential(manifest: &Manifest) -> Result<String, String> {
    fs::create_dir_all(paths::AUTH_ROOT).map_err(|e| e.to_string())?;
    let mut random = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| {
            use std::io::Read;
            file.read_exact(&mut random)
        })
        .map_err(|_| "read credential entropy".to_string())?;
    let token = hex(&random);
    let token_path = Path::new(paths::AUTH_ROOT).join(format!("{}.token", manifest.id));
    sys::atomic_write(&token_path, format!("{token}\n").as_bytes(), 0o600)?;
    let authorization = serde_json::json!({
        "id": manifest.id,
        "token": token,
        "capabilities": manifest.capabilities,
    });
    let mut body = serde_json::to_vec(&authorization).map_err(|e| e.to_string())?;
    body.push(b'\n');
    sys::atomic_write(
        &Path::new(paths::AUTH_ROOT).join(format!("{}.json", manifest.id)),
        &body,
        0o600,
    )?;
    Ok(token_path.to_string_lossy().into_owned())
}

pub fn remove_credential(id: &str) {
    let _ = fs::remove_file(Path::new(paths::AUTH_ROOT).join(format!("{id}.token")));
    let _ = fs::remove_file(Path::new(paths::AUTH_ROOT).join(format!("{id}.json")));
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
