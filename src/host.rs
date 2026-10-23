use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::install;
use crate::manifest::{load_installed, Installed, Manifest};
use crate::paths;
use crate::sandbox::{
    helper_sandbox_arguments, is_active, is_failed, load_kernel_modules, sandbox_arguments,
    systemd_run, systemd_run_with_socket, unit_action, unit_has_dependency,
};
use crate::settings::{
    repair_settings_output_permission, repair_settings_output_permissions, settings_for_state,
    write_runtime_settings,
};
use crate::state::{
    initialize_default_enabled_states, prepare_authorization, prepare_runtime_dirs,
    read_generation, read_state, remove_authorization, write_generation, PluginState,
};
use crate::sys;

pub struct Host {
    pub generation: std::sync::Mutex<String>,
}

struct ReconcileCandidate {
    id: String,
    state: PluginState,
}

struct LegacyStart {
    id: String,
    extension: Installed,
    state: PluginState,
}

impl Host {
    pub fn new() -> Self {
        Self {
            generation: std::sync::Mutex::new(read_generation()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ControllerPlan {
    #[serde(default)]
    actions: Vec<ControllerAction>,
    #[serde(default)]
    response: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ControllerAction {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    environment: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    restart: String,
    #[serde(default)]
    timeout_seconds: u32,
}

pub fn apply(
    host: &Host,
    id: &str,
    revision: u64,
    event: &str,
    reason: &str,
    changed_keys: &[String],
    method: &str,
    request: Option<Value>,
) -> Result<Value, String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    let state = read_state(id)?;
    if revision != 0 && revision != state.revision {
        return Err(format!(
            "stale extension revision {revision}; current revision is {}",
            state.revision
        ));
    }
    let extension = load_installed(id, &state.active_version)?;
    if !method.is_empty() {
        let spec = extension
            .manifest
            .methods
            .get(method)
            .ok_or_else(|| format!("extension {id} does not export method {method:?}"))?;
        if !state.enabled && !extension.manifest.is_environment() && !spec.available_when_disabled {
            return Err(format!(
                "method {method:?} is unavailable while extension {id} is disabled"
            ));
        }
        if !spec.command.is_empty() {
            return apply_command_method(&extension, &state, spec, request);
        }
    }
    if extension.manifest.is_environment() {
        cleanup_disabled_controller(id)?;
        return Ok(Value::Null);
    }
    if extension.manifest.controller.is_none() {
        if manages_legacy_service(&extension.manifest) {
            return apply_legacy(&extension, &state, event);
        }
        if extension.manifest.methods.has_commands() {
            if state.enabled && matches!(event, "enable" | "reconcile" | "config_changed") {
                prepare_enabled_extension(&extension)?;
            }
            stop_helper(id)?;
            stop_legacy(id)?;
            return Ok(Value::Null);
        }
        return apply_legacy(&extension, &state, event);
    }
    apply_controller(
        host,
        &extension,
        &state,
        event,
        reason,
        changed_keys,
        method,
        request,
    )
}

fn apply_command_method(
    extension: &Installed,
    state: &PluginState,
    spec: &crate::manifest::MethodSpec,
    request: Option<Value>,
) -> Result<Value, String> {
    let id = &extension.manifest.id;
    if extension
        .manifest
        .permissions
        .iter()
        .any(|permission| permission == "opkg-admin")
    {
        match spec.name.as_str() {
            "list_packages" => {
                let refresh = request
                    .as_ref()
                    .and_then(|value| value.get("refresh"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if refresh
                    && crate::feed::list_feed_sources()?.iter().any(|source| {
                        source.enabled
                            && source.feeds_text.lines().any(|line| {
                                matches!(line.split_whitespace().next(), Some("src" | "src/gz"))
                            })
                    })
                {
                    sys::with_lock(crate::feed::refresh)?;
                }
                return Ok(marketplace_list_packages()?);
            }
            "list_sources" => {
                return Ok(marketplace_list_sources()?);
            }
            "inspect_source_key" => {
                let public_key = request
                    .as_ref()
                    .and_then(|v| v.get("public_key"))
                    .and_then(Value::as_str)
                    .ok_or("public_key is required")?;
                let key = crate::feed_keys::validate(public_key)?;
                return Ok(json!({"fingerprint": key.fingerprint}));
            }
            "set_source" => {
                marketplace_set_source(request.as_ref())?;
                return Ok(json!({"status": "ok"}));
            }
            "set_source_enabled" => {
                marketplace_set_source_enabled(request.as_ref())?;
                return Ok(json!({"status": "ok"}));
            }
            "remove_source" => {
                crate::feed::remove_custom_source(&marketplace_source_key(request.as_ref())?)?;
                return Ok(json!({"status": "ok"}));
            }
            "install_package" => {
                let package = marketplace_package_name(request.as_ref())?;
                let object = request.as_ref().and_then(Value::as_object);
                let wait = object
                    .and_then(|object| object.get("wait"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let size_bytes = object
                    .and_then(|object| object.get("size_bytes"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                install::install_package_with_options(&package, wait, size_bytes)?;
                if wait {
                    return Ok(serde_json::json!({"status": "ok", "package": package}));
                }
                return Ok(serde_json::json!({"status": "started", "package": package}));
            }
            "install_progress" => {
                return Ok(install::install_progress());
            }
            "remove_package" => {
                let package = marketplace_package_name(request.as_ref())?;
                let delete_data = request
                    .as_ref()
                    .and_then(Value::as_object)
                    .and_then(|object| object.get("delete_data"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if delete_data {
                    let id = package
                        .strip_prefix("onekvm-extension-")
                        .unwrap_or(package.as_str());
                    install::purge(id)?;
                } else {
                    install::remove_package(&package)?;
                }
                return Ok(serde_json::json!({"status": "ok", "package": package}));
            }
            _ => {}
        }
    }
    prepare_runtime_dirs(id)?;
    let settings = settings_for_state(extension, state)?;
    write_runtime_settings(id, &settings)?;
    let command = build_method_command(spec, request.as_ref())?;
    let timeout = if spec.timeout_seconds == 0 {
        10
    } else {
        spec.timeout_seconds
    };
    ensure_helper(extension, Some((&command, timeout)))?;
    let input = serde_json::to_vec(&request.unwrap_or(Value::Null)).map_err(|e| e.to_string())?;
    let mut executable = command;
    executable[0] = extension
        .root
        .join(&executable[0])
        .to_string_lossy()
        .into_owned();
    let result = crate::helper_client::exec_capture(id, &executable, &[], &input, timeout as u32)
        .and_then(|output| {
            serde_json::from_slice(&output).map_err(|e| format!("decode method response: {e}"))
        });
    let repaired = repair_settings_output_permission(id);
    match (result, repaired) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

pub fn check_health(id: &str, revision: u64) -> Result<(), String> {
    let state = read_state(id)?;
    if state.revision != revision {
        return Err("extension changed during health check".into());
    }
    let extension = load_installed(id, &state.active_version)?;
    let health = &extension.manifest.healthcheck;
    if health.type_name.is_empty() {
        return Ok(());
    }
    if health.type_name != "exec" || health.command.is_empty() {
        return Err("unsupported extension health check".into());
    }
    if !state.enabled {
        return Err("health check requires an enabled extension".into());
    }
    validate_command(&extension, &health.command)?;
    prepare_runtime_dirs(id)?;
    write_runtime_settings(id, &settings_for_state(&extension, &state)?)?;
    let timeout = health.timeout_seconds.clamp(1, 30);
    let temporary = ensure_helper(&extension, Some((&health.command, timeout)))?;
    let mut command = health.command.clone();
    command[0] = extension
        .root
        .join(&command[0])
        .to_string_lossy()
        .into_owned();
    let result =
        crate::helper_client::exec_capture(id, &command, &[], b"", timeout as u32).map(|_| ());
    let cleanup = if temporary && extension.manifest.controller.is_none() {
        stop_helper(id)
    } else {
        Ok(())
    };
    result.and(cleanup)
}

fn marketplace_list_sources() -> Result<Value, String> {
    let sources = crate::feed::list_feed_sources()?
        .into_iter()
        .map(|source| {
            json!({
                "key": source.key,
                "name": source.name,
                "path": source.path,
                "feeds_text": source.feeds_text,
                "public_key": source.public_key,
                "key_fingerprint": source.key_fingerprint,
                "builtin": source.builtin,
                "enabled": source.enabled,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "sources": sources }))
}

fn marketplace_source_key(request: Option<&Value>) -> Result<String, String> {
    let object = request.and_then(Value::as_object);
    let key = object
        .and_then(|object| object.get("key"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let key = if key.is_empty() {
        object
            .and_then(|object| object.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
    } else {
        key
    };
    if !paths::valid_feed_source_key(key) {
        return Err("invalid source key".into());
    }
    Ok(key.to_owned())
}

fn marketplace_set_source(request: Option<&Value>) -> Result<(), String> {
    let object = request
        .and_then(Value::as_object)
        .ok_or_else(|| "key, name, and feeds_text are required".to_string())?;
    let key = object
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let key = if key.is_empty() { name } else { key };
    let name = if name.is_empty() { key } else { name };
    let feeds_text = object
        .get("feeds_text")
        .and_then(Value::as_str)
        .ok_or_else(|| "key, name, and feeds_text are required".to_string())?;
    let public_key = object
        .get("public_key")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "public_key must be a string".to_string())
        })
        .transpose()?;
    crate::feed::set_custom_source(key, name, feeds_text, public_key)
}

fn marketplace_set_source_enabled(request: Option<&Value>) -> Result<(), String> {
    let object = request
        .and_then(Value::as_object)
        .ok_or_else(|| "set_source_enabled requires an object".to_string())?;
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| "set_source_enabled requires boolean enabled".to_string())?;
    if object.get("builtin").and_then(Value::as_bool) == Some(true) {
        crate::feed::set_builtin_source_enabled(enabled)?;
        return Ok(());
    }
    crate::feed::set_custom_source_enabled(&marketplace_source_key(request)?, enabled)
}

fn marketplace_package_name(request: Option<&Value>) -> Result<String, String> {
    let object = request
        .and_then(Value::as_object)
        .ok_or_else(|| "install/remove request must be an object".to_string())?;
    if let Some(package) = object.get("package").and_then(Value::as_str) {
        return install::resolve_extension_package(package);
    }
    if let Some(id) = object.get("id").and_then(Value::as_str) {
        return install::resolve_extension_package(id);
    }
    Err("package or id is required".into())
}

const MARKETPLACE_CACHE_VERSION: i32 = 6;

#[derive(serde::Serialize, serde::Deserialize)]
struct MarketplaceDiskCache {
    version: i32,
    fingerprint: String,
    packages: Value,
}

fn marketplace_list_packages() -> Result<Value, String> {
    let mut result = if let Ok(fingerprint) = marketplace_fingerprint() {
        if let Some(cached) = read_marketplace_cache(&fingerprint) {
            cached
        } else {
            let built = build_marketplace_list_packages()?;
            let _ = write_marketplace_cache(&fingerprint, &built);
            built
        }
    } else {
        build_marketplace_list_packages()?
    };
    if let Some(object) = result.as_object_mut() {
        object.insert("storage".into(), marketplace_storage());
    }
    Ok(result)
}

fn marketplace_storage() -> Value {
    let (total, available) = storage_usage(paths::APPS_ROOT)
        .or_else(|| storage_usage("/data"))
        .or_else(|| storage_usage("/"))
        .unwrap_or((0, 0));
    json!({
        "total_bytes": total,
        "available_bytes": available,
        "used_bytes": total.saturating_sub(available),
    })
}

fn storage_usage(path: &str) -> Option<(u64, u64)> {
    let c_path = std::ffi::CString::new(path).ok()?;
    let mut vfs = unsafe { std::mem::zeroed::<libc::statvfs>() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    let frsize = vfs.f_frsize as u64;
    if frsize == 0 || vfs.f_blocks == 0 {
        return None;
    }
    Some((
        (vfs.f_blocks as u64).saturating_mul(frsize),
        (vfs.f_bavail as u64).saturating_mul(frsize),
    ))
}

fn build_marketplace_list_packages() -> Result<Value, String> {
    use crate::manifest::{installed_versions, load_installed, PageIcon};
    use std::collections::BTreeSet;

    let available = crate::feed::available_extensions();
    let mut seen = BTreeSet::new();
    let mut packages = Vec::new();

    let push_manifest = |packages: &mut Vec<Value>,
                         seen: &mut BTreeSet<String>,
                         id: String,
                         manifest: &crate::manifest::Manifest,
                         installed: bool,
                         package_version: &str,
                         icon_data_url: String,
                         size_bytes: u64,
                         installed_size_bytes: u64| {
        if id == "plugin-marketplace" || id == "plugin-market" || !seen.insert(id.clone()) {
            return;
        }
        let installed_version = marketplace_installed_version(&id).unwrap_or_default();
        let installed_package_version = crate::control::installed_package_version(
            &format!("onekvm-extension-{id}"),
            &installed_version,
        )
        .unwrap_or_default();
        let update_available = marketplace_update_available(
            &installed_version,
            &installed_package_version,
            &manifest.version,
            package_version,
        );
        let name = if manifest.name.is_empty() {
            id.clone()
        } else {
            manifest.name.clone()
        };
        let icon = manifest
            .icon
            .clone()
            .or_else(|| manifest.page.as_ref().and_then(|page| page.icon.clone()));
        let mut icon_value = match icon {
            Some(PageIcon { source, name, path }) => {
                let custom = source == "custom";
                let mut value = json!({ "source": source });
                if !name.is_empty() {
                    value["name"] = json!(name);
                }
                if !path.is_empty() {
                    value["path"] = json!(path);
                }
                if custom && !icon_data_url.is_empty() {
                    value["data"] = json!(icon_data_url);
                }
                value
            }
            None => Value::Null,
        };
        if icon_value.is_null() && !icon_data_url.is_empty() {
            icon_value = json!({
                "source": "custom",
                "data": icon_data_url,
            });
        }
        packages.push(json!({
            "id": id,
            "package": format!("onekvm-extension-{id}"),
            "name": name,
            "description": manifest.description,
            "version": manifest.version,
            "kind": manifest.kind,
            "installed": installed,
            "installed_version": installed_version,
            "package_version": package_version,
            "installed_package_version": installed_package_version,
            "update_available": update_available,
            "i18n": manifest.i18n,
            "icon": icon_value,
            "icon_data_url": icon_data_url,
            "size_bytes": size_bytes,
            "installed_size_bytes": installed_size_bytes,
        }));
    };

    for (id, available_extension) in &available {
        let installed = !installed_versions(id)?.is_empty();
        let mut icon_data_url = available_extension.icon_data_url.clone();
        if icon_data_url.is_empty() && installed {
            if let Ok(versions) = installed_versions(id) {
                if let Some(version) = versions.last() {
                    if let Ok(installed_ext) = load_installed(id, version) {
                        icon_data_url = installed_custom_icon_data_url(&installed_ext.manifest);
                    }
                }
            }
        }
        let mut installed_size_bytes = available_extension.installed_size_bytes;
        if installed_size_bytes == 0 && installed {
            if let Ok(versions) = installed_versions(id) {
                if let Some(version) = versions.last() {
                    installed_size_bytes = directory_size(&crate::paths::app_root(id, version));
                }
            }
        }
        push_manifest(
            &mut packages,
            &mut seen,
            id.clone(),
            &available_extension.manifest,
            installed,
            &available_extension.package_version,
            icon_data_url,
            available_extension.size_bytes,
            installed_size_bytes,
        );
    }

    if let Ok(entries) = std::fs::read_dir(crate::paths::APPS_ROOT) {
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if seen.contains(&id) || !crate::paths::valid_id(&id) {
                continue;
            }
            let Ok(versions) = installed_versions(&id) else {
                continue;
            };
            let Some(version) = versions.last() else {
                continue;
            };
            let Ok(installed) = load_installed(&id, version) else {
                continue;
            };
            let icon_data_url = installed_custom_icon_data_url(&installed.manifest);
            push_manifest(
                &mut packages,
                &mut seen,
                id.clone(),
                &installed.manifest,
                true,
                "",
                icon_data_url,
                0,
                directory_size(&crate::paths::app_root(&id, version)),
            );
        }
    }

    packages.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .cmp(&b.get("id").and_then(Value::as_str))
    });
    Ok(json!({ "packages": packages }))
}

fn marketplace_installed_version(id: &str) -> Result<String, String> {
    let versions = crate::manifest::installed_versions(id)?;
    let state = read_state(id)?;
    if versions.contains(&state.active_version) {
        return Ok(state.active_version);
    }
    Ok(versions
        .into_iter()
        .max_by(|a, b| crate::control::compare_versions(a, b))
        .unwrap_or_default())
}

fn marketplace_update_available(
    installed: &str,
    installed_package: &str,
    available: &str,
    available_package: &str,
) -> bool {
    if installed.is_empty() || available_package.is_empty() {
        return false;
    }
    if !installed_package.is_empty() {
        crate::control::compare_versions(available_package, installed_package).is_gt()
    } else {
        // Old/shared payloads may have no matching opkg record after an OS slot switch.
        crate::control::compare_versions(available, installed).is_gt()
    }
}

fn marketplace_fingerprint() -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    use std::time::UNIX_EPOCH;

    fn fingerprint_path(hash: &mut Sha256, path: &Path) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Ok(info) => {
                let nanos = info
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0);
                hash.update(
                    format!(
                        "{} {} {} {:o}\n",
                        path.display(),
                        info.size(),
                        nanos,
                        info.mode()
                    )
                    .as_bytes(),
                );
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                hash.update(format!("absent {}\n", path.display()).as_bytes());
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    let mut hash = Sha256::new();
    fingerprint_path(&mut hash, Path::new(paths::APPS_ROOT))?;
    if let Ok(mut entries) = fs::read_dir(paths::APPS_ROOT) {
        let mut names = Vec::new();
        for entry in entries.by_ref() {
            names.push(entry.map_err(|e| e.to_string())?);
        }
        names.sort_by_key(|entry| entry.file_name());
        for entry in names {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || !paths::valid_id(&id) {
                continue;
            }
            let id_path = Path::new(paths::APPS_ROOT).join(&id);
            fingerprint_path(&mut hash, &id_path)?;
            fingerprint_path(&mut hash, &paths::state_path(&id))?;
            let package = format!("onekvm-extension-{id}");
            fingerprint_path(
                &mut hash,
                &Path::new("/var/lib/opkg/info").join(format!("{package}.control")),
            )?;
            for slot in ["A", "B"] {
                fingerprint_path(
                    &mut hash,
                    &Path::new(paths::OPKG_SLOTS)
                        .join(slot)
                        .join("opkg/info")
                        .join(format!("{package}.control")),
                )?;
            }
            if let Ok(mut versions) = fs::read_dir(&id_path) {
                let mut version_entries = Vec::new();
                for version in versions.by_ref() {
                    version_entries.push(version.map_err(|e| e.to_string())?);
                }
                version_entries.sort_by_key(|entry| entry.file_name());
                for version in version_entries {
                    if version.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        fingerprint_path(
                            &mut hash,
                            &Path::new(paths::EXTENSION_PACKAGE_METADATA)
                                .join(&package)
                                .join(format!("{}.control", version.file_name().to_string_lossy())),
                        )?;
                        fingerprint_path(
                            &mut hash,
                            &id_path.join(version.file_name()).join("manifest.json"),
                        )?;
                    }
                }
            }
        }
    }
    fingerprint_path(&mut hash, Path::new(paths::OPKG_LISTS))?;
    if let Ok(mut lists) = fs::read_dir(paths::OPKG_LISTS) {
        let mut names = Vec::new();
        for entry in lists.by_ref() {
            names.push(entry.map_err(|e| e.to_string())?);
        }
        names.sort_by_key(|entry| entry.file_name());
        for entry in names {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                fingerprint_path(&mut hash, &entry.path())?;
            }
        }
    }
    for path in paths::feed_config_paths() {
        fingerprint_path(&mut hash, &path)?;
    }
    // Dependency estimates change when any system package is installed or removed.
    fingerprint_path(&mut hash, Path::new("/var/lib/opkg/status"))?;
    fingerprint_path(&mut hash, Path::new("/etc/opkg/arch.conf"))?;
    fingerprint_path(&mut hash, Path::new(paths::TRUSTED_FEEDS))?;
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn read_marketplace_cache(fingerprint: &str) -> Option<Value> {
    let data = fs::read(paths::MARKETPLACE_CACHE).ok()?;
    let cache: MarketplaceDiskCache = serde_json::from_slice(&data).ok()?;
    if cache.version != MARKETPLACE_CACHE_VERSION || cache.fingerprint != fingerprint {
        return None;
    }
    if !cache.packages.is_object() {
        return None;
    }
    Some(cache.packages)
}

fn write_marketplace_cache(fingerprint: &str, packages: &Value) -> Result<(), String> {
    let payload = serde_json::to_vec(&MarketplaceDiskCache {
        version: MARKETPLACE_CACHE_VERSION,
        fingerprint: fingerprint.to_owned(),
        packages: packages.clone(),
    })
    .map_err(|e| e.to_string())?;
    let mut payload = payload;
    payload.push(b'\n');
    sys::atomic_write(Path::new(paths::MARKETPLACE_CACHE), &payload, 0o600)
}

fn installed_custom_icon_data_url(manifest: &Manifest) -> String {
    crate::catalog::custom_icon_data_url(manifest)
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total = total.saturating_add(directory_size(&entry.path()));
        } else if file_type.is_file() {
            if let Ok(metadata) = entry.metadata() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

fn build_method_command(
    spec: &crate::manifest::MethodSpec,
    request: Option<&Value>,
) -> Result<Vec<String>, String> {
    let object = match request {
        None | Some(Value::Null) => None,
        Some(Value::Object(_)) if spec.args.is_empty() => None,
        Some(Value::Object(object)) => Some(object),
        _ => return Err(format!("method {:?} request must be an object", spec.name)),
    };
    if let Some(object) = object {
        for name in object.keys() {
            if !spec.args.iter().any(|argument| argument.name == *name) {
                return Err(format!(
                    "method {:?} does not declare argument {name:?}",
                    spec.name
                ));
            }
        }
    }
    let mut command = spec.command.clone();
    let mut omitted_optional = false;
    for argument in &spec.args {
        let value = object.and_then(|values| values.get(&argument.name));
        let Some(value) = value else {
            if argument.required {
                return Err(format!(
                    "method {:?} requires argument {:?}",
                    spec.name, argument.name
                ));
            }
            omitted_optional = true;
            continue;
        };
        if argument.secret {
            if value.as_str().is_none() {
                return Err(format!(
                    "method {:?} argument {:?} must be {}",
                    spec.name, argument.name, argument.type_name
                ));
            }
            continue;
        }
        if omitted_optional {
            return Err(format!(
                "method {:?} argument {:?} cannot follow an omitted optional argument",
                spec.name, argument.name
            ));
        }
        let encoded = match argument.type_name.as_str() {
            "string" => value.as_str().map(str::to_owned),
            "integer" if value.as_i64().is_some() || value.as_u64().is_some() => {
                Some(value.to_string())
            }
            "number" if value.is_number() => Some(value.to_string()),
            "boolean" => value.as_bool().map(|value| value.to_string()),
            _ => None,
        }
        .ok_or_else(|| {
            format!(
                "method {:?} argument {:?} must be {}",
                spec.name, argument.name, argument.type_name
            )
        })?;
        command.push(encoded);
    }
    Ok(command)
}

fn apply_legacy(extension: &Installed, state: &PluginState, event: &str) -> Result<Value, String> {
    match event {
        "disable" => stop_legacy(&extension.manifest.id),
        "enable" | "reconcile" | "config_changed" if state.enabled => {
            start_legacy(extension, state)
        }
        "enable" | "reconcile" | "config_changed" => stop_legacy(&extension.manifest.id),
        _ => Err(format!(
            "extension {} has no controller",
            extension.manifest.id
        )),
    }
    .map(|_| Value::Null)
}

fn apply_controller(
    host: &Host,
    extension: &Installed,
    state: &PluginState,
    event: &str,
    reason: &str,
    changed_keys: &[String],
    method: &str,
    request: Option<Value>,
) -> Result<Value, String> {
    let id = &extension.manifest.id;
    if state.enabled && matches!(event, "enable" | "reconcile" | "config_changed") {
        prepare_enabled_extension(extension)?;
    }
    prepare_runtime_dirs(id)?;
    let settings = settings_for_state(extension, state)?;
    write_runtime_settings(id, &settings)?;
    let helper_started = ensure_helper(extension, None)?;
    let generation = host
        .generation
        .lock()
        .map_err(|_| "host state lock is poisoned".to_string())?
        .clone();
    if helper_started && state.enabled && event == "invoke" {
        let payload = json!({
            "api_version": 1,
            "event": "reconcile",
            "extension_id": id,
            "revision": state.revision,
            "enabled": true,
            "settings": settings,
            "reason": "helper_restarted",
            "core_generation": generation,
        });
        let plan = run_controller(extension, &payload, "reconcile")?;
        execute_plan(extension, &plan)?;
    }
    let mut payload = json!({
        "api_version": 1,
        "event": event,
        "extension_id": id,
        "revision": state.revision,
        "enabled": state.enabled,
        "settings": settings,
        "reason": reason,
        "core_generation": generation,
    });
    if !changed_keys.is_empty() {
        payload["changed_keys"] = json!(changed_keys);
    }
    if !method.is_empty() {
        payload["method"] = json!(method);
    }
    if let Some(request) = request {
        payload["request"] = request;
    }
    let plan = run_controller(extension, &payload, event)?;
    execute_plan(extension, &plan)?;
    repair_settings_output_permission(id)?;
    if !state.enabled {
        stop_helper(id)?;
    }
    Ok(plan.response.unwrap_or(Value::Null))
}

pub fn reconcile_all(host: &Host, generation: &str) -> Result<(), String> {
    write_generation(generation)?;
    *host
        .generation
        .lock()
        .map_err(|_| "host state lock is poisoned".to_string())? = generation.to_owned();
    sys::with_lock(initialize_default_enabled_states)?;
    let (mut candidates, mut failures) = read_candidates()?;
    prioritize_enabled(&mut candidates);
    let (legacy, serial) = split_legacy_starts(candidates, &mut failures);
    start_legacy_parallel(&legacy, &mut failures);
    for candidate in serial {
        if let Err(err) = reconcile_one(host, &candidate.id, &candidate.state) {
            failures.push(format!("{}: {err}", candidate.id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("reconcile failures: {}", failures.join("; ")))
    }
}

// Start enabled legacy services as soon as Core has created the extension
// control and media sockets. ReconcileAll remains the authoritative readiness
// barrier and will retry any failed or incomplete prewarm operation.
pub fn prewarm() -> Result<(), String> {
    eprintln!("extension prewarm: discovering enabled legacy services");
    sys::with_lock(initialize_default_enabled_states)?;
    let (mut candidates, mut failures) = read_candidates()?;
    prioritize_enabled(&mut candidates);
    let (legacy, _) = split_legacy_starts(candidates, &mut failures);
    start_legacy_parallel(&legacy, &mut failures);
    if failures.is_empty() {
        eprintln!("extension prewarm: enabled legacy services are active");
        Ok(())
    } else {
        Err(format!("prewarm failures: {}", failures.join("; ")))
    }
}

fn read_candidates() -> Result<(Vec<ReconcileCandidate>, Vec<String>), String> {
    let entries = match fs::read_dir(paths::CONFIG_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()))
        }
        Err(err) => return Err(err.to_string()),
    };
    let mut candidates = Vec::new();
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
            Ok(state) => candidates.push(ReconcileCandidate {
                id: id.to_owned(),
                state,
            }),
            Err(err) => failures.push(format!("{id}: {err}")),
        }
    }
    Ok((candidates, failures))
}

fn split_legacy_starts(
    candidates: Vec<ReconcileCandidate>,
    failures: &mut Vec<String>,
) -> (Vec<LegacyStart>, Vec<ReconcileCandidate>) {
    let mut legacy = Vec::new();
    let mut serial = Vec::new();
    for candidate in candidates {
        if !candidate.state.enabled || candidate.state.active_version.is_empty() {
            serial.push(candidate);
            continue;
        }
        match load_installed(&candidate.id, &candidate.state.active_version) {
            Ok(extension) if manages_legacy_service(&extension.manifest) => {
                legacy.push(LegacyStart {
                    id: candidate.id,
                    extension,
                    state: candidate.state,
                });
            }
            Ok(_) => serial.push(candidate),
            Err(err) => failures.push(format!("{}: {err}", candidate.id)),
        }
    }
    (legacy, serial)
}

fn start_legacy_parallel(starts: &[LegacyStart], failures: &mut Vec<String>) {
    const MAX_PARALLEL_STARTS: usize = 2;
    for batch in starts.chunks(MAX_PARALLEL_STARTS) {
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(batch.len());
            for start in batch {
                handles.push((
                    start.id.as_str(),
                    scope.spawn(move || start_legacy(&start.extension, &start.state)),
                ));
            }
            for (id, handle) in handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => failures.push(format!("{id}: {err}")),
                    Err(_) => failures.push(format!("{id}: start worker panicked")),
                }
            }
        });
    }
}

fn prioritize_enabled(candidates: &mut [ReconcileCandidate]) {
    candidates.sort_by_key(|candidate| !candidate.state.enabled);
}

fn reconcile_one(host: &Host, id: &str, state: &PluginState) -> Result<(), String> {
    if state.active_version.is_empty() {
        return Ok(());
    }
    let extension = load_installed(id, &state.active_version)?;
    if extension.manifest.is_environment() {
        return cleanup_disabled_controller(id);
    }
    if manages_legacy_service(&extension.manifest) {
        if state.enabled {
            start_legacy(&extension, &state)
        } else {
            stop_legacy(id)
        }
    } else if extension.manifest.controller.is_some() && state.enabled {
        apply_controller(
            host,
            &extension,
            &state,
            "reconcile",
            "core_ready",
            &[],
            "",
            None,
        )
        .map(|_| ())
    } else {
        if state.enabled {
            prepare_enabled_extension(&extension)?;
        }
        cleanup_disabled_controller(id)
    }
}

// Controller actions can only supervise processes inside the per-extension
// helper.  Once that helper and any old transient units are stopped, a
// disabled extension has no runtime state left to reconcile.  Avoid spawning a
// fully sandboxed helper merely to return remove_all during every cold boot.
fn cleanup_disabled_controller(id: &str) -> Result<(), String> {
    stop_extension_runtime(id)?;
    remove_authorization(id);
    let runtime = Path::new(paths::RUNTIME_ROOT).join(id);
    match fs::remove_dir_all(runtime) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("remove disabled extension runtime: {err}")),
    }
}

pub fn validate_device_variant(manifest: &Manifest) -> Result<(), String> {
    if !manifest.device_compatibility.is_empty() {
        let machine = current_machine()?;
        for compatibility in &manifest.device_compatibility {
            if compatibility.machine != machine {
                continue;
            }
            if compatibility.variants.is_empty() {
                return Ok(());
            }
            return validate_current_variant(&manifest.id, &compatibility.variants);
        }
        return Err(format!(
            "extension {} does not support machine {machine}",
            manifest.id
        ));
    }
    if manifest.device_variants.is_empty() {
        return Ok(());
    }
    validate_current_variant(&manifest.id, &manifest.device_variants)
}

fn current_machine() -> Result<String, String> {
    let data = fs::read(paths::MACHINE_PROFILE).map_err(|_| "read machine identity".to_string())?;
    let value: Value =
        serde_json::from_slice(&data).map_err(|_| "machine identity is unavailable".to_string())?;
    let machine = value
        .get("machine")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if paths::valid_id(machine) {
        Ok(machine.to_owned())
    } else {
        Err("machine identity is unavailable".into())
    }
}

fn validate_current_variant(id: &str, variants: &[String]) -> Result<(), String> {
    let data = fs::read(paths::MACHINE_HARDWARE).map_err(|_| "read device variant".to_string())?;
    let value: Value =
        serde_json::from_slice(&data).map_err(|_| "device variant is unavailable".to_string())?;
    let variant = value
        .get("variant")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if variants.iter().any(|candidate| candidate == variant) {
        Ok(())
    } else {
        Err(format!(
            "extension {id} does not support device variant {variant}"
        ))
    }
}

fn manages_legacy_service(manifest: &Manifest) -> bool {
    manifest.controller.is_none() && !manifest.entrypoint.is_empty()
}

// Module loading belongs to the privileged D-Bus host. Manager CLI callers
// include Core, whose service intentionally does not have CAP_SYS_MODULE.
fn prepare_enabled_extension(extension: &Installed) -> Result<(), String> {
    validate_device_variant(&extension.manifest)?;
    validate_conflicts(&extension.manifest.id, &extension.manifest)?;
    load_kernel_modules(extension)
}

fn start_legacy(extension: &Installed, state: &PluginState) -> Result<(), String> {
    eprintln!(
        "extension {}: preparing legacy service",
        extension.manifest.id
    );
    prepare_enabled_extension(extension)?;
    let unit = paths::unit_name(&extension.manifest.id);
    let socket_unit = paths::socket_unit(&extension.manifest.id);
    if extension.manifest.socket_activation.is_some() && is_active(&socket_unit) {
        return Ok(());
    }
    if is_active(&unit) {
        if extension.manifest.socket_activation.is_none() && legacy_unit_has_core_lifetime(&unit) {
            return Ok(());
        }
        eprintln!(
            "extension {}: replacing legacy PartOf unit with ordered BindsTo unit",
            extension.manifest.id
        );
        unit_action("stop", &unit)?;
        let _ = unit_action("reset-failed", &unit);
    }
    prepare_runtime_dirs(&extension.manifest.id)?;
    let settings = settings_for_state(extension, state)?;
    write_runtime_settings(&extension.manifest.id, &settings)?;
    if !service_enabled(&settings) {
        return Ok(());
    }
    let listen_address = socket_listen_address(&extension.manifest, &settings)?;
    prepare_authorization(&extension.manifest)?;
    let mut arguments = vec![
        "--quiet".into(),
        "--collect".into(),
        format!("--unit={}", paths::unit_name(&extension.manifest.id)),
        format!("--slice={}", paths::EXTENSIONS_SLICE),
        "--property=Restart=on-failure".into(),
        "--property=RestartSec=1s".into(),
    ];
    arguments.extend(legacy_core_lifetime_properties());
    arguments.extend(sandbox_arguments(extension));
    arguments.extend([
        "--setenv=CREDENTIALS_DIRECTORY=/usr/share/onekvm/extension-compat".into(),
        format!("--working-directory={}", extension.root.display()),
        format!("--setenv=ONEKVM_EXTENSION_ID={}", extension.manifest.id),
        format!(
            "--setenv=ONEKVM_EXTENSION_DATA={}",
            Path::new(paths::APP_DATA_ROOT)
                .join(&extension.manifest.id)
                .display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_CONFIG={}",
            Path::new(paths::RUNTIME_ROOT)
                .join(&extension.manifest.id)
                .display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_SOCKET={}",
            Path::new(paths::RUNTIME_ROOT)
                .join(&extension.manifest.id)
                .join("sockets")
                .display()
        ),
        "--".into(),
        extension
            .root
            .join(&extension.manifest.entrypoint)
            .to_string_lossy()
            .into_owned(),
    ]);
    eprintln!(
        "extension {}: submitting transient service",
        extension.manifest.id
    );
    let socket = listen_address
        .as_deref()
        .map(|address| (socket_unit.as_str(), address));
    if let Err(err) = systemd_run_with_socket(&arguments, socket) {
        let _ = stop_systemd_unit(&socket_unit);
        let _ = stop_systemd_unit(&unit);
        remove_authorization(&extension.manifest.id);
        let _ = fs::remove_dir_all(Path::new(paths::RUNTIME_ROOT).join(&extension.manifest.id));
        return Err(err);
    }
    std::thread::sleep(Duration::from_millis(200));
    let active_unit = if socket.is_some() {
        &socket_unit
    } else {
        &unit
    };
    if !is_active(active_unit) {
        /* Restart=on-failure may already have queued a retry. Stop the unit
         * before removing its authorization record and runtime directories;
         * otherwise queued retries obscure the original process error. */
        let _ = unit_action("stop", active_unit);
        if is_failed(&unit) {
            let _ = unit_action("reset-failed", &unit);
        }
        remove_authorization(&extension.manifest.id);
        let _ = fs::remove_dir_all(Path::new(paths::RUNTIME_ROOT).join(&extension.manifest.id));
        return Err(format!(
            "extension {} exited during startup",
            extension.manifest.id
        ));
    }
    eprintln!(
        "extension {}: legacy service is active",
        extension.manifest.id
    );
    Ok(())
}

fn legacy_unit_has_core_lifetime(unit: &str) -> bool {
    unit_has_dependency(unit, "BindsTo", "onekvm.service")
        && unit_has_dependency(unit, "After", "onekvm.service")
}

fn legacy_core_lifetime_properties() -> [String; 2] {
    /* BindsTo stops the extension when Core disappears, while After prevents
     * systemd from automatically restarting it during the middle of a Core
     * restart. The host then creates the new service after Core is active and
     * authorization is registered before starting its service. PartOf propagates
     * the Core restart job and can race authorization preparation. */
    [
        "--property=BindsTo=onekvm.service".into(),
        "--property=After=onekvm.service".into(),
    ]
}

fn stop_legacy(id: &str) -> Result<(), String> {
    unit_action("stop", &paths::socket_unit(id))?;
    let unit = paths::unit_name(id);
    unit_action("stop", &unit)?;
    if is_failed(&unit) {
        let _ = unit_action("reset-failed", &unit);
    }
    remove_authorization(id);
    let _ = fs::remove_dir_all(Path::new(paths::RUNTIME_ROOT).join(id));
    Ok(())
}

fn service_enabled(settings: &Value) -> bool {
    match settings.get("service_enabled") {
        None => true,
        Some(Value::Bool(value)) => *value,
        _ => true,
    }
}

fn socket_listen_address(manifest: &Manifest, settings: &Value) -> Result<Option<String>, String> {
    let Some(socket) = &manifest.socket_activation else {
        return Ok(None);
    };
    let address = settings
        .get(&socket.address_setting)
        .and_then(Value::as_str)
        .ok_or_else(|| "socket activation bind address is missing".to_string())?
        .parse::<std::net::IpAddr>()
        .map_err(|_| {
            "socket activation bind address must be an IPv4 or IPv6 address".to_string()
        })?;
    let port = settings
        .get(&socket.port_setting)
        .and_then(Value::as_u64)
        .filter(|port| (1..=65535).contains(port))
        .ok_or_else(|| "socket activation port must be between 1 and 65535".to_string())?;
    Ok(Some(
        std::net::SocketAddr::new(address, port as u16).to_string(),
    ))
}

pub fn validate_conflicts(id: &str, manifest: &Manifest) -> Result<(), String> {
    if manifest.kind == "cloud-provider" {
        validate_cloud_provider_conflict(id)?;
    }
    if manifest.activation_conflicts.is_empty() {
        return Ok(());
    }
    let machine = current_machine()?;
    for conflict in &manifest.activation_conflicts {
        if !conflict.machines.is_empty() && !conflict.machines.iter().any(|item| item == &machine) {
            continue;
        }
        let state = read_state(&conflict.extension)?;
        if state.enabled {
            return Err(format!(
                "extension {id} conflicts with enabled extension {} on machine {machine}",
                conflict.extension
            ));
        }
    }
    Ok(())
}

fn validate_cloud_provider_conflict(id: &str) -> Result<(), String> {
    let entries = match fs::read_dir(paths::APPS_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries.flatten() {
        let candidate = entry.file_name().to_string_lossy().into_owned();
        if candidate == id
            || !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            || !paths::valid_id(&candidate)
        {
            continue;
        }
        let state = read_state(&candidate)?;
        if !state.enabled || state.active_version.is_empty() {
            continue;
        }
        let Ok(extension) = load_installed(&candidate, &state.active_version) else {
            continue;
        };
        if extension.manifest.kind == "cloud-provider" {
            return Err(format!(
                "cloud provider {id} conflicts with enabled cloud provider {candidate}"
            ));
        }
    }
    Ok(())
}

fn ensure_helper(extension: &Installed, direct: Option<(&[String], u64)>) -> Result<bool, String> {
    let id = &extension.manifest.id;
    let command_only = direct.is_some() && extension.manifest.controller.is_none();
    let mode = if command_only {
        "command"
    } else {
        "controller"
    };
    let reuse_key = format!("sandbox-v3:{}:{mode}", extension.manifest.version);
    if helper_ready(id, &reuse_key) {
        return Ok(false);
    }
    stop_helper(id)?;
    if !manages_legacy_service(&extension.manifest) {
        stop_legacy_unit(id)?;
        stop_legacy_controller_units(id)?;
    }
    prepare_authorization(&extension.manifest)?;
    let address = paths::helper_address(id);
    let _ = fs::remove_file(&address);
    let (controller_command, controller_timeout, idle_timeout) =
        if let Some(controller) = &extension.manifest.controller {
            let mut command = vec![controller.entrypoint.clone()];
            command.extend(controller.arguments.iter().cloned());
            (command, controller.timeout_seconds, 0)
        } else if let Some((command, timeout)) = direct {
            (command.to_vec(), timeout, 60)
        } else {
            let controller = extension
                .manifest
                .controller
                .as_ref()
                .ok_or_else(|| format!("extension {id} has no controller"))?;
            let mut command = vec![controller.entrypoint.clone()];
            command.extend(controller.arguments.iter().cloned());
            (command, controller.timeout_seconds, 0)
        };
    let mut arguments = vec![
        "--quiet".into(),
        "--collect".into(),
        format!("--unit={}", paths::helper_unit(id)),
        format!("--slice={}", paths::EXTENSIONS_SLICE),
        "--property=Restart=no".into(),
        "--property=KillMode=control-group".into(),
        "--property=TimeoutStopSec=5s".into(),
        "--property=BindsTo=onekvm.service".into(),
        "--property=After=onekvm.service".into(),
    ];
    arguments.extend(helper_sandbox_arguments(extension));
    arguments.extend([
        "--setenv=CREDENTIALS_DIRECTORY=/usr/share/onekvm/extension-compat".into(),
        format!("--working-directory={}", extension.root.display()),
        format!("--setenv=ONEKVM_EXTENSION_ID={id}"),
        format!(
            "--setenv=ONEKVM_EXTENSION_DATA={}",
            Path::new(paths::APP_DATA_ROOT).join(id).display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_CONFIG={}",
            Path::new(paths::RUNTIME_ROOT).join(id).display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_SOCKET={}",
            Path::new(paths::RUNTIME_ROOT)
                .join(id)
                .join("sockets")
                .display()
        ),
        "--".into(),
        paths::HOST_BINARY.into(),
        "--plugin-helper".into(),
        id.clone(),
        controller_timeout.to_string(),
        address.to_string_lossy().into_owned(),
        reuse_key.clone(),
        idle_timeout.to_string(),
        extension
            .root
            .join(&controller_command[0])
            .to_string_lossy()
            .into_owned(),
    ]);
    arguments.extend(controller_command.into_iter().skip(1));
    systemd_run(&arguments).map_err(|err| format!("start extension helper: {err}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if helper_ready(id, &reuse_key) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = stop_helper(id);
    Err(format!("extension helper {id} did not become ready"))
}

fn helper_ready(id: &str, reuse_key: &str) -> bool {
    let Ok(address) = fs::read_to_string(paths::helper_address(id)) else {
        return false;
    };
    if address.lines().nth(1) != Some(reuse_key) {
        return false;
    }
    call_helper(id, "Ping", &[])
        .ok()
        .and_then(|value| value.get(0).and_then(Value::as_str).map(|text| text == id))
        .unwrap_or(false)
}

pub fn declared_service_units(manifest: &Manifest) -> Vec<String> {
    let mut units = Vec::new();
    for service in &manifest.services {
        if service.unit.is_empty() {
            continue;
        }
        if !paths::valid_extension_service_unit(&manifest.id, &service.unit) {
            continue;
        }
        if !units.iter().any(|existing| existing == &service.unit) {
            units.push(service.unit.clone());
        }
    }
    units
}

pub fn stop_declared_service_units(manifest: &Manifest, disable: bool) -> Result<(), String> {
    for unit in declared_service_units(manifest) {
        stop_systemd_unit(&unit)?;
        if disable {
            let _ = unit_action("disable", &unit);
        }
    }
    Ok(())
}

pub fn stop_extension_runtime(id: &str) -> Result<(), String> {
    stop_helper(id)?;
    stop_legacy_unit(id)?;
    stop_legacy_controller_units(id)
}

fn stop_systemd_unit(unit: &str) -> Result<(), String> {
    if is_active(unit) {
        unit_action("stop", unit).map_err(|err| format!("stop {unit}: {err}"))?;
    }
    if is_failed(unit) {
        let _ = unit_action("reset-failed", unit);
    }
    Ok(())
}

fn stop_helper(id: &str) -> Result<(), String> {
    let unit = paths::helper_unit(id);
    let address = paths::helper_address(id);
    if !address.exists() && !Path::new(paths::TRANSIENT_ROOT).join(&unit).exists() {
        return Ok(());
    }
    if is_active(&unit) {
        unit_action("stop", &unit)?;
    } else if is_failed(&unit) {
        let _ = unit_action("reset-failed", &unit);
    }
    let _ = fs::remove_file(address);
    Ok(())
}

fn stop_legacy_unit(id: &str) -> Result<(), String> {
    // Stop the listener first, otherwise pending connections can reactivate
    // the service while it is being disabled or upgraded.
    unit_action("stop", &paths::socket_unit(id))?;
    let unit = paths::unit_name(id);
    if !Path::new(paths::TRANSIENT_ROOT).join(&unit).exists() {
        return Ok(());
    }
    if is_active(&unit) {
        unit_action("stop", &unit)?;
    }
    if is_failed(&unit) {
        let _ = unit_action("reset-failed", &unit);
    }
    Ok(())
}

fn stop_legacy_controller_units(id: &str) -> Result<(), String> {
    let helper = paths::helper_unit(id);
    let mut legacy_units = Vec::new();
    if let Ok(entries) = fs::read_dir(paths::TRANSIENT_ROOT) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&format!("onekvm-extension-{id}-"))
                && name.ends_with(".service")
                && name != helper
            {
                legacy_units.push(name.into_owned());
            }
        }
    }
    legacy_units.sort();
    for unit in legacy_units {
        stop_systemd_unit(&unit)?;
    }
    remove_legacy_slice(id)
}

fn remove_legacy_slice(id: &str) -> Result<(), String> {
    let name = format!(
        "system-onekvm-extensions-{}.slice",
        id.replace('-', r"\x2d")
    );
    let path = Path::new(paths::RUNTIME_UNIT_ROOT).join(&name);
    if path.exists() {
        let _ = unit_action("stop", &name);
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn run_controller(
    extension: &Installed,
    payload: &Value,
    event: &str,
) -> Result<ControllerPlan, String> {
    let encoded = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let output = call_helper_bytes(&extension.manifest.id, "RunController", &encoded)?;
    if output.len() > 1 << 20 {
        return Err("controller output exceeds 1 MiB".into());
    }
    let plan: ControllerPlan =
        serde_json::from_slice(&output).map_err(|e| format!("decode controller plan: {e}"))?;
    if plan.actions.len() > 32 {
        return Err("controller plan contains too many actions".into());
    }
    let _ = event;
    Ok(plan)
}

fn execute_plan(extension: &Installed, plan: &ControllerPlan) -> Result<(), String> {
    for action in &plan.actions {
        execute_action(extension, action)
            .map_err(|err| format!("controller action {}/{}: {err}", action.kind, action.name))?;
    }
    Ok(())
}

fn execute_action(extension: &Installed, action: &ControllerAction) -> Result<(), String> {
    if action.kind != "remove_all" && !paths::valid_method_name(&action.name) {
        return Err(format!("invalid action instance name {:?}", action.name));
    }
    match action.kind.as_str() {
        "ensure" => ensure_instance(extension, action),
        "remove" => {
            call_helper(&extension.manifest.id, "Remove", &[json!(action.name)]).map(|_| ())
        }
        "remove_all" => call_helper(&extension.manifest.id, "RemoveAll", &[]).map(|_| ()),
        "exec" => exec_action(extension, action),
        other => Err(format!("unsupported action type {other:?}")),
    }
}

fn ensure_instance(extension: &Installed, action: &ControllerAction) -> Result<(), String> {
    validate_command(extension, &action.command)?;
    let restart = if action.restart.is_empty() {
        "on-failure"
    } else {
        action.restart.as_str()
    };
    if !matches!(restart, "no" | "on-failure" | "always") {
        return Err(format!("unsupported restart policy {restart:?}"));
    }
    let mut command = action.command.clone();
    command[0] = extension
        .root
        .join(&action.command[0])
        .to_string_lossy()
        .into_owned();
    let mut environment = Vec::new();
    for (key, value) in &action.environment {
        environment.push(format!("{key}={value}"));
    }
    call_helper(
        &extension.manifest.id,
        "Ensure",
        &[
            json!(action.name),
            json!(command),
            json!(environment),
            json!(restart),
        ],
    )
    .map(|_| ())
}

fn exec_action(extension: &Installed, action: &ControllerAction) -> Result<(), String> {
    validate_command(extension, &action.command)?;
    let timeout = if action.timeout_seconds == 0 {
        10
    } else {
        action.timeout_seconds
    };
    if !(1..=30).contains(&timeout) {
        return Err("action timeout must be between 1 and 30 seconds".into());
    }
    let mut command = action.command.clone();
    command[0] = extension
        .root
        .join(&action.command[0])
        .to_string_lossy()
        .into_owned();
    let environment: Vec<String> = action
        .environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    call_helper(
        &extension.manifest.id,
        "Exec",
        &[json!(command), json!(environment), json!(timeout)],
    )
    .map(|_| ())
}

fn validate_command(extension: &Installed, command: &[String]) -> Result<(), String> {
    if command.is_empty() || command[0].starts_with('/') || command[0].contains("..") {
        return Err("action command must start with a safe extension-relative executable".into());
    }
    let path = extension.root.join(&command[0]);
    let meta = fs::metadata(&path).map_err(|_| {
        format!(
            "action executable {} is missing or escapes extension root",
            command[0]
        )
    })?;
    if !meta.is_file() {
        return Err(format!(
            "action executable {} is not executable",
            command[0]
        ));
    }
    Ok(())
}

fn call_helper(id: &str, method: &str, args: &[Value]) -> Result<Vec<Value>, String> {
    crate::helper_client::call(id, method, args)
}

fn call_helper_bytes(id: &str, method: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    if method != "RunController" {
        return Err("unsupported helper byte method".into());
    }
    crate::helper_client::run_controller(id, payload)
}

pub fn core_lost(host: &Host) {
    if let Ok(mut generation) = host.generation.lock() {
        generation.clear();
    }
    let _ = fs::remove_file(paths::GENERATION_PATH);
    // Socket units also have BindsTo=onekvm.service, but Core can lose its
    // bus name before systemd sees it exit. Prevent reactivation in that gap.
    if let Ok(entries) = fs::read_dir(paths::TRANSIENT_ROOT) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("onekvm-extension-") && name.ends_with(".socket") {
                let _ = unit_action("stop", &name);
            }
        }
    }
    let _ = unit_action("stop", paths::EXTENSIONS_SLICE);
}

pub fn repair_permissions() -> Result<(), String> {
    repair_settings_output_permissions()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marketplace_updates_require_a_newer_installable_package() {
        assert!(marketplace_update_available(
            "1.9", "1.9-r9", "1.10", "1.10-r0"
        ));
        assert!(marketplace_update_available(
            "1.0", "1.0-r2", "1.0", "1.0-r10"
        ));
        assert!(!marketplace_update_available(
            "1.0", "1.0-r10", "1.0", "1.0-r2"
        ));
        assert!(!marketplace_update_available(
            "1.0", "1.0-r2", "1.0", "1.0-r2"
        ));
        assert!(!marketplace_update_available(
            "1.0",
            "1.0-r2.debug20261014",
            "1.0",
            "1.0-r2"
        ));
        assert!(!marketplace_update_available("", "", "1.0", "1.0-r0"));
        assert!(!marketplace_update_available("1.0", "1.0-r0", "1.0", ""));
        assert!(marketplace_update_available("1.9", "", "1.10", "1.10-r0"));
        assert!(!marketplace_update_available("1.0", "", "1.0", "1.0-r10"));
    }

    #[test]
    fn socket_activation_only_accepts_numeric_tcp_addresses() {
        let manifest = Manifest {
            socket_activation: Some(crate::manifest::SocketActivation {
                address_setting: "listen_address".into(),
                port_setting: "port".into(),
            }),
            ..Manifest::default()
        };
        for (address, expected) in [("0.0.0.0", "0.0.0.0:5900"), ("::1", "[::1]:5900")] {
            assert_eq!(
                socket_listen_address(&manifest, &json!({"listen_address":address,"port":5900}))
                    .unwrap()
                    .as_deref(),
                Some(expected)
            );
        }
        for settings in [
            json!({"listen_address":"localhost","port":5900}),
            json!({"listen_address":"0.0.0.0\nService=other","port":5900}),
            json!({"listen_address":"0.0.0.0","port":0}),
            json!({"listen_address":"::","port":65536}),
            json!({"port":5900}),
        ] {
            assert!(socket_listen_address(&manifest, &settings).is_err());
        }
        assert_eq!(
            socket_listen_address(&Manifest::default(), &json!({})).unwrap(),
            None
        );
    }

    fn candidate(id: &str, enabled: bool) -> ReconcileCandidate {
        ReconcileCandidate {
            id: id.into(),
            state: PluginState {
                enabled,
                ..PluginState::default()
            },
        }
    }

    #[test]
    fn declared_service_units_keep_namespaced_systemd_units() {
        let mut manifest = Manifest::default();
        manifest.id = "samba".into();
        manifest.services = vec![
            crate::manifest::ManifestService {
                id: "manager".into(),
                unit: "onekvm-samba-manager.service".into(),
                ..crate::manifest::ManifestService::default()
            },
            crate::manifest::ManifestService {
                id: "helper".into(),
                unit: String::new(),
                ..crate::manifest::ManifestService::default()
            },
            crate::manifest::ManifestService {
                id: "unsafe".into(),
                unit: "sshd.service".into(),
                ..crate::manifest::ManifestService::default()
            },
        ];
        assert_eq!(
            declared_service_units(&manifest),
            vec!["onekvm-samba-manager.service"]
        );
    }

    #[test]
    fn command_methods_can_keep_a_legacy_entrypoint() {
        let mut manifest = Manifest::default();
        assert!(!manages_legacy_service(&manifest));
        manifest.entrypoint = "bin/daemon".into();
        assert!(manages_legacy_service(&manifest));
        manifest.controller = Some(crate::manifest::ControllerSpec {
            runtime: "executable".into(),
            entrypoint: "bin/daemon".into(),
            arguments: vec!["--controller".into()],
            timeout_seconds: 5,
        });
        assert!(!manages_legacy_service(&manifest));
    }

    #[test]
    fn legacy_service_lifetime_is_ordered_after_core() {
        let properties = legacy_core_lifetime_properties();
        assert_eq!(
            properties,
            [
                "--property=BindsTo=onekvm.service",
                "--property=After=onekvm.service",
            ]
        );
        assert!(!properties.iter().any(|value| value.contains("PartOf=")));
    }

    #[test]
    fn reconcile_prioritizes_enabled_extensions() {
        let mut candidates = vec![
            candidate("disabled-a", false),
            candidate("enabled-a", true),
            candidate("disabled-b", false),
            candidate("enabled-b", true),
        ];
        prioritize_enabled(&mut candidates);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            vec!["enabled-a", "enabled-b", "disabled-a", "disabled-b"]
        );
    }

    #[test]
    fn method_arguments_are_validated_and_appended_in_order() {
        let spec = crate::manifest::MethodSpec {
            name: "save".into(),
            command: vec!["bin/editor".into(), "save".into()],
            args: vec![
                crate::manifest::MethodArgument {
                    name: "id".into(),
                    type_name: "string".into(),
                    required: true,
                    secret: false,
                },
                crate::manifest::MethodArgument {
                    name: "force".into(),
                    type_name: "boolean".into(),
                    required: true,
                    secret: false,
                },
            ],
            ..crate::manifest::MethodSpec::default()
        };
        let request = json!({"force": true, "id": "display"});
        assert_eq!(
            build_method_command(&spec, Some(&request)).unwrap(),
            ["bin/editor", "save", "display", "true"]
        );
        assert!(build_method_command(&spec, Some(&json!({"id": "display"}))).is_err());
    }

    #[test]
    fn secret_method_arguments_are_validated_but_not_put_in_argv() {
        let spec = crate::manifest::MethodSpec {
            name: "login".into(),
            command: vec!["bin/client".into(), "login".into()],
            args: vec![
                crate::manifest::MethodArgument {
                    name: "username".into(),
                    type_name: "string".into(),
                    required: true,
                    secret: false,
                },
                crate::manifest::MethodArgument {
                    name: "password".into(),
                    type_name: "string".into(),
                    required: true,
                    secret: true,
                },
            ],
            ..crate::manifest::MethodSpec::default()
        };
        let request = json!({"username": "admin", "password": "not-in-argv"});
        assert_eq!(
            build_method_command(&spec, Some(&request)).unwrap(),
            ["bin/client", "login", "admin"]
        );
        assert!(
            build_method_command(&spec, Some(&json!({"username": "admin", "password": true})))
                .is_err()
        );
    }

    #[test]
    fn stdin_only_methods_accept_undeclared_object_keys() {
        let spec = crate::manifest::MethodSpec {
            name: "confirm".into(),
            command: vec!["bin/onekvm-boot-confirm".into()],
            ..crate::manifest::MethodSpec::default()
        };
        let request = json!({
            "version": 1,
            "action": "login",
            "username": "admin",
            "challenge_id": "pending",
            "cancel_path": "/run/onekvm/mfa/pending.cancel",
            "response": {"kind": "plugin"}
        });
        assert_eq!(
            build_method_command(&spec, Some(&request)).unwrap(),
            ["bin/onekvm-boot-confirm"]
        );
    }

    #[test]
    fn marketplace_source_key_rejects_invalid_ids() {
        assert_eq!(
            marketplace_source_key(Some(&json!({"name": "mirror"}))).unwrap(),
            "mirror"
        );
        assert_eq!(
            marketplace_source_key(Some(&json!({"key": "lab", "name": "内部镜像"}))).unwrap(),
            "lab"
        );
        assert!(marketplace_source_key(Some(&json!({"name": "Bad"}))).is_err());
        assert!(marketplace_source_key(Some(&json!({"key": "Bad"}))).is_err());
        assert!(marketplace_source_key(Some(&json!({"name": ""}))).is_err());
        assert!(marketplace_set_source(Some(&json!({"name": "mirror"}))).is_err());
        assert!(marketplace_set_source(Some(&json!({
            "key": "lab",
            "name": "内部镜像"
        })))
        .is_err());
        assert!(marketplace_set_source(Some(&json!({
            "name": "mirror",
            "feeds_text": "not a feed"
        })))
        .is_err());
        assert!(marketplace_set_source(Some(&json!({
            "key": "Bad",
            "name": "内部镜像",
            "feeds_text": "src a https://example.org/a\n"
        })))
        .is_err());
        assert!(marketplace_set_source_enabled(Some(&json!({"name": "mirror"}))).is_err());
        assert!(marketplace_set_source_enabled(Some(&json!({
            "key": "lab",
            "enabled": "yes"
        })))
        .is_err());
    }
}
