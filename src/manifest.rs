use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths;

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[allow(dead_code)]
pub struct Manifest {
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub manifest_version: i32,
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub i18n: std::collections::BTreeMap<String, Translation>,
    pub version: String,
    #[serde(default)]
    pub api_version: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub architecture: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    #[serde(default)]
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_modules: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<ManifestVideo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_variants: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_compatibility: Vec<DeviceCompatibility>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default_enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub memory_budget: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activation_conflicts: Vec<ActivationConflict>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ManifestService>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PageIcon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<ExtensionPage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolbar: Option<ExtensionToolbar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<ExtensionRoute>,
    #[serde(default)]
    pub lifecycle: LifecycleHooks,
    pub controller: Option<ControllerSpec>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub methods: std::collections::BTreeMap<String, MethodSpec>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub settings_reload: String,
    #[serde(default)]
    pub settings_schema: SettingsSchema,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_fields: Vec<String>,
    #[serde(default)]
    pub resource_limits: ResourceLimits,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<ManifestDirectory>,
    #[serde(default)]
    pub healthcheck: Healthcheck,
}

fn is_zero_i32(value: &i32) -> bool {
    *value == 0
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Translation {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ManifestVideo {
    #[serde(default)]
    pub settings: String,
    #[serde(default)]
    pub mode_select: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ManifestService {
    pub id: String,
    #[serde(default)]
    pub i18n: std::collections::BTreeMap<String, Translation>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct PageIcon {
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ExtensionPage {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub renderer: String,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PageIcon>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ExtensionToolbar {
    pub entrypoint: String,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ExtensionRoute {
    pub path: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub socket: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub port: i32,
    #[serde(default)]
    pub auth: String,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ManifestDirectory {
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    pub path: String,
    pub location: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub writable: bool,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct DeviceCompatibility {
    pub machine: String,
    #[serde(default)]
    pub variants: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ActivationConflict {
    pub extension: String,
    #[serde(default)]
    pub machines: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct LifecycleHooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ControllerSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub runtime: String,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct MethodSpec {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub available_when_disabled: bool,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct SettingsSchema {
    #[serde(default, rename = "type")]
    pub type_name: String,
    #[serde(default, rename = "additionalProperties")]
    pub additional_properties: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    #[serde(default)]
    pub properties: std::collections::BTreeMap<String, SettingProperty>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct SettingProperty {
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub i18n: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, rename = "enum", skip_serializing_if = "Vec::is_empty")]
    pub enum_values: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(default, rename = "maxLength", skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    #[serde(default, rename = "readOnly", skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_when: Option<SettingCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_when: Option<SettingCondition>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SettingCondition {
    pub field: String,
    pub equals: Value,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ResourceLimits {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub memory_max: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cpu_quota: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub tasks_max: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub limit_nofile: i64,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Healthcheck {
    #[serde(default, rename = "type", skip_serializing_if = "String::is_empty")]
    pub type_name: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub timeout_seconds: u64,
}

pub struct Installed {
    pub manifest: Manifest,
    pub root: PathBuf,
}

pub fn load_installed(id: &str, version: &str) -> Result<Installed, String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    let version = if version.is_empty() {
        let versions = installed_versions(id)?;
        versions
            .last()
            .cloned()
            .ok_or_else(|| format!("extension {id} is not installed"))?
    } else {
        version.to_owned()
    };
    if !paths::valid_version(&version) {
        return Err(format!("invalid extension version {version:?}"));
    }
    let root = paths::app_root(id, &version);
    let data = fs::read(root.join("manifest.json")).map_err(|e| format!("read manifest: {e}"))?;
    let manifest: Manifest =
        serde_json::from_slice(&data).map_err(|e| format!("decode manifest: {e}"))?;
    if manifest.id != id || manifest.version != version {
        return Err("manifest identity does not match installation path".into());
    }
    Ok(Installed { manifest, root })
}

pub fn installed_versions(id: &str) -> Result<Vec<String>, String> {
    let entries = match fs::read_dir(Path::new(paths::APPS_ROOT).join(id)) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.to_string()),
    };
    let mut versions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && paths::valid_version(&entry.file_name().to_string_lossy())
        {
            versions.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    versions.sort();
    Ok(versions)
}

pub fn decode_manifest(data: &[u8]) -> Result<Manifest, String> {
    let mut manifest: Manifest =
        serde_json::from_slice(data).map_err(|e| format!("decode manifest: {e}"))?;
    if let Some(page) = &mut manifest.page {
        if page.renderer.is_empty() {
            page.renderer = "html".into();
        }
    }
    if manifest.api_version == 2 {
        if manifest.i18n.is_empty() && !manifest.name.is_empty() {
            let description = if manifest.description.trim().is_empty() {
                "OneKVM extension".into()
            } else {
                manifest.description.clone()
            };
            manifest.i18n.insert(
                "default".into(),
                Translation {
                    name: manifest.name.clone(),
                    description,
                },
            );
        }
        if let Some(fallback) = manifest.i18n.get("default") {
            manifest.name = fallback.name.clone();
            manifest.description = fallback.description.clone();
        }
    }
    validate_manifest(&manifest)?;
    if manifest.api_version == 1 {
        manifest.name = manifest.id.clone();
        manifest.description = "OneKVM extension".into();
        manifest.kind = "service".into();
        manifest.settings_schema = SettingsSchema {
            type_name: "object".into(),
            ..SettingsSchema::default()
        };
    }
    if manifest.settings_schema.type_name.is_empty() {
        manifest.settings_schema.type_name = "object".into();
    }
    Ok(manifest)
}

pub fn runtime_architecture() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64".into(),
        "aarch64" => "aarch64".into(),
        other => other.into(),
    }
}

fn validate_manifest(value: &Manifest) -> Result<(), String> {
    if value.manifest_version < 0 || value.manifest_version > 1 {
        return Err(format!("unsupported manifest version {}", value.manifest_version));
    }
    if !paths::valid_id(&value.id) {
        return Err(format!("invalid extension id {:?}", value.id));
    }
    if !paths::valid_version(&value.version) {
        return Err(format!("invalid extension version {:?}", value.version));
    }
    if value.api_version != 1 && value.api_version != 2 {
        return Err(format!("unsupported plugin API version {}", value.api_version));
    }
    if !value.author.is_empty()
        && (value.author.trim() != value.author || value.author.len() > 120)
    {
        return Err(format!("invalid author {:?}", value.author));
    }
    if !value.homepage.is_empty() && !valid_homepage(&value.homepage) {
        return Err(format!("invalid homepage {:?}", value.homepage));
    }
    if !value.memory_budget.is_empty() {
        crate::memory::parse_memory_size(&value.memory_budget)
            .map_err(|_| format!("invalid memory_budget {:?}", value.memory_budget))?;
    }
    let architecture = runtime_architecture();
    if !value.architecture.is_empty() && value.architecture != architecture {
        return Err(format!(
            "manifest architecture {:?} does not match {architecture:?}",
            value.architecture
        ));
    }
    if !paths::safe_relative(&value.entrypoint) {
        return Err(format!("unsafe entrypoint {:?}", value.entrypoint));
    }
    if value.api_version == 2 {
        if value.name.trim().is_empty() {
            return Err("name is required".into());
        }
        if value.kind.trim().is_empty() {
            return Err("kind is required".into());
        }
    }
    validate_directories(&value.directories)?;
    if let Some(toolbar) = &value.toolbar {
        if !paths::safe_relative(&toolbar.entrypoint)
            || !toolbar.entrypoint.starts_with("web/")
            || !toolbar.entrypoint.ends_with(".js")
        {
            return Err(format!("unsafe toolbar entrypoint {:?}", toolbar.entrypoint));
        }
    }
    Ok(())
}

fn valid_homepage(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty() && !host.contains('@')
}

fn validate_directories(directories: &[ManifestDirectory]) -> Result<(), String> {
    if directories.len() > 8 {
        return Err("extension may declare at most 8 directories".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for directory in directories {
        if !paths::valid_id(&directory.id) {
            return Err(format!("invalid directory id {:?}", directory.id));
        }
        if !seen.insert(&directory.id) {
            return Err(format!("duplicate directory id {:?}", directory.id));
        }
        if !paths::valid_directory_kind(&directory.kind) {
            return Err(format!("invalid directory kind {:?}", directory.kind));
        }
        if !directory.role.is_empty() && !paths::valid_directory_kind(&directory.role) {
            return Err(format!("invalid directory role {:?}", directory.role));
        }
        if !paths::safe_relative(&directory.path) {
            return Err(format!("unsafe directory path {:?}", directory.path));
        }
        if directory.location != "package" && directory.location != "data" {
            return Err(format!(
                "directory {:?} location must be package or data",
                directory.id
            ));
        }
        if directory.writable && directory.location != "data" {
            return Err(format!(
                "directory {:?} is writable but not stored in data",
                directory.id
            ));
        }
    }
    Ok(())
}
