use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::paths;

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub entrypoint: String,
    #[serde(default)]
    pub kernel_modules: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub device_variants: Vec<String>,
    #[serde(default)]
    pub device_compatibility: Vec<DeviceCompatibility>,
    #[serde(default)]
    pub default_enabled: bool,
    #[serde(default)]
    pub memory_budget: String,
    #[serde(default)]
    pub activation_conflicts: Vec<ActivationConflict>,
    #[serde(default)]
    pub lifecycle: LifecycleHooks,
    pub controller: Option<ControllerSpec>,
    #[serde(default)]
    pub methods: std::collections::BTreeMap<String, MethodSpec>,
    #[serde(default)]
    pub settings_schema: SettingsSchema,
    #[serde(default)]
    pub resource_limits: ResourceLimits,
    #[serde(default)]
    pub healthcheck: Healthcheck,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct DeviceCompatibility {
    pub machine: String,
    #[serde(default)]
    pub variants: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ActivationConflict {
    pub extension: String,
    #[serde(default)]
    pub machines: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct LifecycleHooks {
    #[serde(default)]
    pub enable: Vec<String>,
    #[serde(default)]
    pub disable: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ControllerSpec {
    pub entrypoint: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct MethodSpec {
    #[serde(default)]
    pub available_when_disabled: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SettingsSchema {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub properties: std::collections::BTreeMap<String, SettingProperty>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SettingProperty {
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default, rename = "enum")]
    pub enum_values: Vec<Value>,
    #[serde(default)]
    pub minimum: Option<f64>,
    #[serde(default)]
    pub maximum: Option<f64>,
    #[serde(default, rename = "maxLength")]
    pub max_length: Option<usize>,
    #[serde(default, rename = "readOnly")]
    pub read_only: bool,
    #[serde(default)]
    pub required_when: Option<SettingCondition>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SettingCondition {
    pub field: String,
    pub equals: Value,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ResourceLimits {
    #[serde(default)]
    pub memory_max: String,
    #[serde(default)]
    pub cpu_quota: String,
    #[serde(default)]
    pub tasks_max: i64,
    #[serde(default)]
    pub limit_nofile: i64,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Healthcheck {
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
