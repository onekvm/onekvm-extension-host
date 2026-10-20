use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::feed::available_extensions;
use crate::manifest::{
    installed_versions, load_installed, ActivationConflict, AuthProvider, ExtensionPage,
    ExtensionRoute, ExtensionShell, ExtensionToolbar, ExtensionToolbox, Manifest,
    ManifestDirectory, ManifestVideo, PageIcon, SettingsSchema, Translation,
};
use crate::memory;
use crate::paths;
use crate::sandbox;
use crate::settings;
use crate::state::read_state;
use crate::system_plugin::load_system_plugins;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CatalogService {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub i18n: BTreeMap<String, Translation>,
    pub unit: String,
    pub running: bool,
    pub enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CatalogSummary {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub i18n: BTreeMap<String, Translation>,
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    pub installed: bool,
    pub enabled: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PageIcon>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon_data_url: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_page: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_toolbar: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolbar: Option<ExtensionToolbar>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_shell: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ExtensionShell>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolbox: Option<ExtensionToolbox>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_settings: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<CatalogService>,
    #[serde(skip_serializing_if = "is_zero")]
    pub memory_budget_bytes: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub memory_budget_used_bytes: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub memory_budget_total_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<ManifestDirectory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_provider: Option<AuthProvider>,
}

#[derive(Debug, Serialize, Clone)]
pub struct CatalogEntry {
    pub permissions: Vec<String>,
    pub kernel_modules: Vec<String>,
    pub package_dependencies: Option<Vec<String>>,
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub i18n: BTreeMap<String, Translation>,
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<String>,
    pub installed: bool,
    pub enabled: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<ManifestVideo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub device_variants: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub activation_conflicts: Vec<ActivationConflict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<CatalogService>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PageIcon>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<ExtensionPage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolbar: Option<ExtensionToolbar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ExtensionShell>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolbox: Option<ExtensionToolbox>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<ExtensionRoute>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_provider: Option<AuthProvider>,
    pub settings_schema: SettingsSchema,
    pub settings: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets_configured: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub memory_budget_bytes: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub memory_budget_used_bytes: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub memory_budget_total_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<ManifestDirectory>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CatalogDiskCache {
    version: i32,
    fingerprint: String,
    entries: Vec<serde_json::Value>,
}

const CATALOG_CACHE_VERSION: i32 = 8;
const CATALOG_RUNTIME_TTL: Duration = Duration::from_secs(2);

pub fn catalog() -> Result<Vec<CatalogSummary>, String> {
    if let Ok(fingerprint) = catalog_fingerprint() {
        if let Some(entries) = read_cache(Path::new(paths::CATALOG_CACHE), &fingerprint) {
            return Ok(entries);
        }
        let entries = build_catalog()?;
        let _ = write_cache(&fingerprint, &entries);
        return Ok(entries);
    }
    build_catalog()
}

pub fn build_catalog() -> Result<Vec<CatalogSummary>, String> {
    // Feed packages that are not installed belong in the plugin marketplace, not
    // the local plugin manager list.
    let system_plugins = load_system_plugins()?;
    let mut result = Vec::new();
    match fs::read_dir(paths::APPS_ROOT) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|e| e.to_string())?;
                let id = entry.file_name().to_string_lossy().into_owned();
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || !paths::valid_id(&id)
                {
                    continue;
                }
                match installed_summary(&id) {
                    Ok(None) => {}
                    Ok(Some(item)) => result.push(item),
                    Err(err) => result.push(CatalogSummary {
                        id,
                        error: err,
                        ..empty_summary()
                    }),
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.to_string()),
    }
    result.extend(
        system_plugins
            .into_iter()
            .map(|(_, plugin)| plugin.summary()),
    );
    result.sort_by(|a, b| a.id.cmp(&b.id));
    apply_memory(&mut result)?;
    Ok(result)
}

fn installed_summary(id: &str) -> Result<Option<CatalogSummary>, String> {
    let versions = installed_versions(id)?;
    if versions.is_empty() {
        return Ok(None);
    }
    let state = read_state(id)?;
    let extension = load_installed(id, &state.active_version)?;
    let mut item = summary_from_manifest(
        &extension.manifest,
        true,
        custom_icon_data_url(&extension.manifest),
    );
    item.enabled = state.enabled && !extension.manifest.is_environment();
    if item.enabled {
        item.directories = directories_for_installed(id, &extension.manifest.directories)?;
    }
    item.running = extension_running(id, &extension.manifest, state.enabled);
    item.services = catalog_services(&extension.manifest, id, true, state.enabled);
    Ok(Some(item))
}

pub fn detail(id: &str) -> Result<CatalogEntry, String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    let system_plugins = load_system_plugins()?;
    if let Some(plugin) = system_plugins.get(id) {
        return plugin.entry();
    }
    let versions = installed_versions(id)?;
    if versions.is_empty() {
        let extension = available_extensions()
            .remove(id)
            .ok_or_else(|| format!("extension {id:?} not found"))?;
        let mut item = entry_from_manifest(
            &extension.manifest,
            false,
            vec![extension.manifest.version.clone()],
        );
        item.package_dependencies = Some(extension.package_dependencies);
        return Ok(item);
    }
    let state = read_state(id)?;
    let installed = load_installed(id, &state.active_version)?;
    let mut settings = settings::settings_for_state(&installed, &state)?;
    settings::merge_output(&installed, &mut settings)?;
    let mut item = entry_from_manifest(&installed.manifest, true, versions);
    item.enabled = state.enabled && !installed.manifest.is_environment();
    if item.enabled {
        item.directories = directories_for_installed(id, &installed.manifest.directories)?;
    }
    item.running = extension_running(id, &installed.manifest, state.enabled);
    item.services = catalog_services(&installed.manifest, id, true, state.enabled);
    let (redacted, secrets) = settings::redact(&installed.manifest, &settings);
    item.settings = redacted;
    item.secrets_configured = secrets;
    Ok(item)
}

#[derive(Deserialize)]
struct DirectoriesOutput {
    directories: Vec<ManifestDirectory>,
}

fn directories_for_installed(
    id: &str,
    declared: &[ManifestDirectory],
) -> Result<Vec<ManifestDirectory>, String> {
    let path = Path::new(paths::APP_DATA_ROOT)
        .join(id)
        .join(paths::DIRECTORIES_OUTPUT);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(declared.to_vec()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(declared.to_vec())
        }
        Err(err) => return Err(format!("read dynamic extension directories: {err}")),
    };
    if !metadata.file_type().is_file() {
        return Err("dynamic extension directories output is not a regular file".into());
    }
    if metadata.size() > 64 << 10 {
        return Err("dynamic extension directories output exceeds 64 KiB".into());
    }
    let data = fs::read(&path).map_err(|e| format!("read dynamic extension directories: {e}"))?;
    decode_dynamic_directories(&data, declared)
}

fn decode_dynamic_directories(
    data: &[u8],
    declared: &[ManifestDirectory],
) -> Result<Vec<ManifestDirectory>, String> {
    let output: DirectoriesOutput = serde_json::from_slice(data)
        .map_err(|e| format!("decode dynamic extension directories: {e}"))?;
    if output.directories.len() > 32 {
        return Err("extension may publish at most 32 dynamic directories".into());
    }
    let mut result = declared.to_vec();
    let mut ids = declared
        .iter()
        .map(|directory| directory.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for directory in &output.directories {
        if directory.location != "data" {
            return Err(format!(
                "dynamic directory {:?} location must be data",
                directory.id
            ));
        }
        if !crate::manifest::valid_directory_name(&directory.name)
            || !paths::valid_id(&directory.id)
            || !ids.insert(directory.id.clone())
            || !paths::valid_directory_kind(&directory.kind)
            || (!directory.role.is_empty() && !paths::valid_directory_kind(&directory.role))
            || !paths::safe_relative(&directory.path)
        {
            return Err(format!(
                "invalid or duplicate dynamic directory {:?}",
                directory.id
            ));
        }
        result.push(directory.clone());
    }
    Ok(result)
}

pub fn summary_from_manifest(
    extension: &Manifest,
    installed: bool,
    icon_data_url: String,
) -> CatalogSummary {
    let icon = extension
        .icon
        .clone()
        .or_else(|| extension.page.as_ref().and_then(|page| page.icon.clone()));
    let budget = if extension.memory_budget.is_empty() {
        0
    } else {
        memory::parse_memory_size(&extension.memory_budget).unwrap_or(0)
    };
    CatalogSummary {
        id: extension.id.clone(),
        name: extension.name.clone(),
        description: extension.description.clone(),
        i18n: extension.i18n.clone(),
        kind: extension.kind.clone(),
        author: extension.author.clone(),
        homepage: extension.homepage.clone(),
        version: extension.version.clone(),
        installed,
        enabled: false,
        running: false,
        system: false,
        error: String::new(),
        icon,
        icon_data_url,
        has_page: extension.page.is_some(),
        has_toolbar: extension.toolbar.is_some(),
        toolbar: extension.toolbar.clone(),
        has_shell: extension.shell.is_some(),
        shell: extension.shell.clone(),
        toolbox: extension.toolbox.clone(),
        has_settings: !extension.settings_schema.properties.is_empty(),
        services: catalog_services(extension, &extension.id, installed, false),
        memory_budget_bytes: budget,
        memory_budget_used_bytes: 0,
        memory_budget_total_bytes: 0,
        directories: extension.directories.clone(),
        auth_provider: extension.auth_provider.clone(),
    }
}

pub fn custom_icon_data_url(manifest: &Manifest) -> String {
    use std::path::Path;

    let icon = manifest
        .icon
        .as_ref()
        .or_else(|| manifest.page.as_ref().and_then(|page| page.icon.as_ref()));
    let Some(icon) = icon else {
        return String::new();
    };
    if icon.source != "custom" || icon.path.is_empty() {
        return String::new();
    }
    let mime = match Path::new(&icon.path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => return String::new(),
    };
    let path = Path::new(paths::APPS_ROOT)
        .join(&manifest.id)
        .join(&manifest.version)
        .join(&icon.path);
    let Ok(meta) = fs::metadata(&path) else {
        return String::new();
    };
    if !meta.is_file() || meta.len() == 0 || meta.len() > 256 * 1024 {
        return String::new();
    }
    let Ok(content) = fs::read(&path) else {
        return String::new();
    };
    format!("data:{mime};base64,{}", crate::ipk::base64_encode(&content))
}

fn entry_from_manifest(
    extension: &Manifest,
    installed: bool,
    versions: Vec<String>,
) -> CatalogEntry {
    let budget = if extension.memory_budget.is_empty() {
        0
    } else {
        memory::parse_memory_size(&extension.memory_budget).unwrap_or(0)
    };
    let status = memory::current_status().unwrap_or_default();
    let mut schema = extension.settings_schema.clone();
    if schema.type_name.is_empty() {
        schema.type_name = "object".into();
    }
    CatalogEntry {
        permissions: extension.permissions.clone(),
        kernel_modules: extension.kernel_modules.clone(),
        package_dependencies: if installed {
            crate::control::installed_package_dependencies(
                &format!("onekvm-extension-{}", extension.id),
                &extension.version,
            )
        } else {
            None
        },
        id: extension.id.clone(),
        name: extension.name.clone(),
        description: extension.description.clone(),
        i18n: extension.i18n.clone(),
        kind: extension.kind.clone(),
        author: extension.author.clone(),
        homepage: extension.homepage.clone(),
        version: extension.version.clone(),
        versions,
        installed,
        enabled: false,
        running: false,
        system: false,
        error: String::new(),
        capabilities: extension.capabilities.clone(),
        video: extension.video.clone(),
        device_variants: extension.device_variants.clone(),
        activation_conflicts: extension.activation_conflicts.clone(),
        services: catalog_services(extension, &extension.id, installed, false),
        icon: extension.icon.clone(),
        page: extension.page.clone(),
        toolbar: extension.toolbar.clone(),
        shell: extension.shell.clone(),
        toolbox: extension.toolbox.clone(),
        routes: extension.routes.clone(),
        auth_provider: extension.auth_provider.clone(),
        settings_schema: schema,
        settings: serde_json::json!({}),
        secrets_configured: Vec::new(),
        memory_budget_bytes: budget,
        memory_budget_used_bytes: status.used_bytes,
        memory_budget_total_bytes: status.total_bytes,
        directories: extension.directories.clone(),
    }
}

fn catalog_services(
    extension: &Manifest,
    id: &str,
    installed: bool,
    enabled: bool,
) -> Vec<CatalogService> {
    let mut running_instances = BTreeMap::new();
    if installed && extension.controller.is_some() {
        if let Ok(names) = crate::helper_client::call(id, "Ping", &[]) {
            if !names.is_empty() {
                running_instances.insert("main".to_string(), true);
            }
        }
    }
    extension
        .services
        .iter()
        .map(|service| {
            let translation = service.i18n.get("default").cloned().unwrap_or_default();
            let external_unit = !service.unit.is_empty();
            let unit = if external_unit {
                service.unit.clone()
            } else if extension.controller.is_some() {
                paths::helper_unit(id)
            } else if extension.socket_activation.is_some() {
                paths::socket_unit(id)
            } else {
                paths::unit_name(id)
            };
            let running = if !installed {
                false
            } else if extension.controller.is_some() {
                running_instances.contains_key(&service.id) || sandbox::is_active(&unit)
            } else {
                sandbox::is_active(&unit)
            };
            let service_enabled = if installed && external_unit {
                sandbox::is_enabled(&unit)
            } else {
                enabled
            };
            CatalogService {
                id: service.id.clone(),
                name: translation.name,
                description: translation.description,
                i18n: service.i18n.clone(),
                unit,
                running,
                enabled: service_enabled,
            }
        })
        .collect()
}

pub fn command_only_extension(manifest: &Manifest) -> bool {
    manifest.controller.is_none()
        && manifest.entrypoint.is_empty()
        && manifest.methods.has_commands()
}

/// A package whose process is never started. The plugin list should say it is
/// enabled, not stopped, because there is no service that can be down.
fn resident_service_expected(manifest: &Manifest) -> bool {
    if manifest.controller.is_some()
        || manifest
            .services
            .iter()
            .any(|service| !service.unit.is_empty())
    {
        return true;
    }
    if manifest.entrypoint.is_empty() {
        return false;
    }
    !service_locked_off(manifest)
}

fn service_locked_off(manifest: &Manifest) -> bool {
    let Some(property) = manifest.settings_schema.properties.get("service_enabled") else {
        return false;
    };
    property.read_only && property.default.as_ref().and_then(|value| value.as_bool()) == Some(false)
}

pub fn extension_running(id: &str, manifest: &Manifest, enabled: bool) -> bool {
    if !enabled || manifest.is_environment() {
        return false;
    }
    if command_only_extension(manifest) || !resident_service_expected(manifest) {
        return true;
    }
    if manifest.controller.is_some() {
        sandbox::is_active(&paths::helper_unit(id))
    } else if manifest.socket_activation.is_some() {
        sandbox::is_active(&paths::socket_unit(id))
    } else {
        sandbox::is_active(&paths::unit_name(id))
    }
}

fn empty_summary() -> CatalogSummary {
    CatalogSummary {
        id: String::new(),
        name: String::new(),
        description: String::new(),
        i18n: BTreeMap::new(),
        kind: String::new(),
        author: String::new(),
        homepage: String::new(),
        version: String::new(),
        installed: false,
        enabled: false,
        running: false,
        system: false,
        error: String::new(),
        icon: None,
        icon_data_url: String::new(),
        has_page: false,
        has_toolbar: false,
        toolbar: None,
        has_shell: false,
        shell: None,
        toolbox: None,
        has_settings: false,
        services: Vec::new(),
        memory_budget_bytes: 0,
        memory_budget_used_bytes: 0,
        memory_budget_total_bytes: 0,
        directories: Vec::new(),
        auth_provider: None,
    }
}

fn apply_memory(result: &mut [CatalogSummary]) -> Result<(), String> {
    let status = memory::current_status()?;
    for item in result.iter_mut() {
        item.memory_budget_used_bytes = status.used_bytes;
        item.memory_budget_total_bytes = status.total_bytes;
    }
    Ok(())
}

fn catalog_fingerprint() -> Result<String, String> {
    let mut hash = Sha256::new();
    write_fingerprint(&mut hash)?;
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn write_fingerprint(hash: &mut Sha256) -> Result<(), String> {
    fingerprint_path(hash, Path::new(paths::CONTRIBUTIONS_GENERATION))?;
    fingerprint_path(hash, Path::new(paths::GENERATION_PATH))?;
    fingerprint_path(hash, Path::new(paths::APPS_ROOT))?;
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
            fingerprint_path(hash, &id_path)?;
            if let Ok(mut versions) = fs::read_dir(&id_path) {
                let mut version_entries = Vec::new();
                for version in versions.by_ref() {
                    version_entries.push(version.map_err(|e| e.to_string())?);
                }
                version_entries.sort_by_key(|entry| entry.file_name());
                for version in version_entries {
                    if version.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        fingerprint_path(
                            hash,
                            &id_path.join(version.file_name()).join("manifest.json"),
                        )?;
                    }
                }
            }
            fingerprint_path(hash, &paths::state_path(&id))?;
            fingerprint_path(
                hash,
                &Path::new(paths::APP_DATA_ROOT)
                    .join(&id)
                    .join(paths::DIRECTORIES_OUTPUT),
            )?;
        }
    }
    fingerprint_path(hash, Path::new(paths::CONFIG_ROOT))?;
    fingerprint_path(hash, Path::new(paths::SYSTEM_PLUGINS))?;
    if let Ok(mut entries) = fs::read_dir(paths::SYSTEM_PLUGINS) {
        let mut names = Vec::new();
        for entry in entries.by_ref() {
            names.push(entry.map_err(|e| e.to_string())?);
        }
        names.sort_by_key(|entry| entry.file_name());
        for entry in names {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                continue;
            }
            if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
                fingerprint_path(hash, &entry.path())?;
            }
        }
    }
    fingerprint_path(hash, Path::new(paths::OPKG_LISTS))?;
    if let Ok(mut lists) = fs::read_dir(paths::OPKG_LISTS) {
        let mut names = Vec::new();
        for entry in lists.by_ref() {
            names.push(entry.map_err(|e| e.to_string())?);
        }
        names.sort_by_key(|entry| entry.file_name());
        for entry in names {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                fingerprint_path(hash, &entry.path())?;
            }
        }
    }
    for path in paths::feed_config_paths() {
        fingerprint_path(hash, &path)?;
    }
    fingerprint_path(hash, Path::new(paths::MACHINE_PROFILE))
}

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

fn read_cache(path: &Path, fingerprint: &str) -> Option<Vec<CatalogSummary>> {
    // Service readiness can change without modifying a manifest or settings.
    // Expire on demand; an idle system does not need a polling task.
    let age = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age >= CATALOG_RUNTIME_TTL {
        return None;
    }
    let data = fs::read(path).ok()?;
    let cache: CatalogDiskCache = serde_json::from_slice(&data).ok()?;
    if cache.version != CATALOG_CACHE_VERSION || cache.fingerprint != fingerprint {
        return None;
    }
    serde_json::from_value(serde_json::Value::Array(cache.entries)).ok()
}

fn write_cache(fingerprint: &str, entries: &[CatalogSummary]) -> Result<(), String> {
    let payload = serde_json::to_vec(&CatalogDiskCache {
        version: CATALOG_CACHE_VERSION,
        fingerprint: fingerprint.to_owned(),
        entries: entries
            .iter()
            .filter_map(|entry| serde_json::to_value(entry).ok())
            .collect(),
    })
    .map_err(|e| e.to_string())?;
    let mut payload = payload;
    payload.push(b'\n');
    crate::sys::atomic_write(Path::new(paths::CATALOG_CACHE), &payload, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_runtime_status_expires_and_rejects_clock_rollback() {
        let path = std::env::temp_dir().join(format!(
            "onekvm-catalog-cache-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let payload = serde_json::to_vec(&CatalogDiskCache {
            version: CATALOG_CACHE_VERSION,
            fingerprint: "installed".into(),
            entries: Vec::new(),
        }).unwrap();
        fs::write(&path, payload).unwrap();
        assert!(read_cache(&path, "installed").is_some());
        assert!(read_cache(&path, "changed").is_none());
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() - CATALOG_RUNTIME_TTL - Duration::from_secs(1)).unwrap();
        assert!(read_cache(&path, "installed").is_none());
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(30)).unwrap();
        assert!(read_cache(&path, "installed").is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dynamic_directories_are_data_only_and_merge_with_declared_entries() {
        let declared = vec![ManifestDirectory {
            name: String::new(),
            id: "built-in".into(),
            kind: "documents".into(),
            role: String::new(),
            path: "documents".into(),
            location: "data".into(),
            writable: true,
        }];
        let data = br#"{
            "directories": [{
                "id": "gateway-0011223344556677",
                "name": "Lab NAS",
                "kind": "virtual-media",
                "role": "gateway",
                "path": "gateways/gateway-0011223344556677",
                "location": "data",
                "writable": false
            }]
        }"#;
        let result = decode_dynamic_directories(data, &declared).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].kind, "virtual-media");
        assert_eq!(result[1].name, "Lab NAS");
    }

    #[test]
    fn command_only_auth_plugin_is_ready_when_enabled() {
        let mut methods = std::collections::BTreeMap::new();
        methods.insert(
            "confirm".into(),
            crate::manifest::MethodSpec {
                name: "confirm".into(),
                command: vec!["bin/onekvm-boot-confirm".into()],
                ..crate::manifest::MethodSpec::default()
            },
        );
        let mut manifest = Manifest {
            methods: crate::manifest::Methods(methods),
            ..Manifest::default()
        };
        assert!(command_only_extension(&manifest));
        assert!(extension_running("nanokvm-boot-confirm", &manifest, true));
        assert!(!extension_running("nanokvm-boot-confirm", &manifest, false));

        manifest.entrypoint = "bin/daemon".into();
        assert!(!command_only_extension(&manifest));
    }

    #[test]
    fn extensions_without_a_resident_process_are_ready_when_enabled() {
        let page_only = Manifest::default();
        assert!(extension_running("edid-editor", &page_only, true));
        assert!(!extension_running("edid-editor", &page_only, false));

        let mut property = crate::manifest::SettingProperty::default();
        property.read_only = true;
        property.default = Some(serde_json::Value::Bool(false));
        let mut stub = Manifest {
            entrypoint: "bin/onekvm-example-toolbox".into(),
            ..Manifest::default()
        };
        stub.settings_schema
            .properties
            .insert("service_enabled".into(), property);
        assert!(extension_running("example-toolbox", &stub, true));
        assert!(!extension_running("example-toolbox", &stub, false));

        let daemon = Manifest {
            entrypoint: "bin/daemon".into(),
            ..Manifest::default()
        };
        assert!(!extension_running("example-daemon", &daemon, true));
    }

    #[test]
    fn dynamic_directories_reject_package_paths_and_duplicate_ids() {
        let package = br#"{
            "directories": [{
                "id": "gateway-a",
                "kind": "virtual-media",
                "path": "media",
                "location": "package"
            }]
        }"#;
        assert!(decode_dynamic_directories(package, &[]).is_err());

        let declared = vec![ManifestDirectory {
            name: String::new(),
            id: "gateway-a".into(),
            kind: "virtual-media".into(),
            role: String::new(),
            path: "media".into(),
            location: "data".into(),
            writable: false,
        }];
        let duplicate = br#"{
            "directories": [{
                "id": "gateway-a",
                "kind": "virtual-media",
                "path": "other",
                "location": "data"
            }]
        }"#;
        assert!(decode_dynamic_directories(duplicate, &declared).is_err());
    }
    #[test]
    fn directory_names_are_optional_and_reject_control_characters() {
        let base = serde_json::json!({"directories":[{
            "id":"gateway-test", "kind":"virtual-media", "path":"gateways/test", "location":"data"
        }]});
        let decode = |value: &serde_json::Value| {
            decode_dynamic_directories(&serde_json::to_vec(value).unwrap(), &[])
        };
        assert!(decode(&base).unwrap()[0].name.is_empty());
        let mut named = base.clone();
        named["directories"][0]["name"] = serde_json::json!("资料库 NAS");
        assert_eq!(decode(&named).unwrap()[0].name, "资料库 NAS");
        for name in ["bad\nname".to_owned(), "x".repeat(256)] {
            named["directories"][0]["name"] = serde_json::json!(name);
            assert!(decode(&named).is_err());
        }
    }
}
