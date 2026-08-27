use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::catalog::{CatalogEntry, CatalogService, CatalogSummary};
use crate::manifest::{ExtensionPage, PageIcon, SettingsSchema, Translation};
use crate::paths;
use crate::sandbox;

#[derive(Debug, Clone, Deserialize)]
pub struct SystemPlugin {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub i18n: BTreeMap<String, Translation>,
    pub kind: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub icon: Option<PageIcon>,
    #[serde(default)]
    pub page: Option<ExtensionPage>,
    pub unit: String,
    #[serde(default)]
    pub settings_schema: SettingsSchema,
    #[serde(default)]
    pub dbus: Option<SystemPluginDBus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemPluginDBus {
    pub bus_name: String,
    pub object_path: String,
    pub interface: String,
    pub get_settings: String,
    pub update_settings: String,
}

impl SystemPlugin {
    pub fn summary(&self) -> CatalogSummary {
        let running = sandbox::is_active(&self.unit);
        let enabled = sandbox::is_enabled(&self.unit);
        let icon = self
            .icon
            .clone()
            .or_else(|| self.page.as_ref().and_then(|page| page.icon.clone()));
        CatalogSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            i18n: self.i18n.clone(),
            kind: self.kind.clone(),
            author: String::new(),
            homepage: String::new(),
            version: self.version.clone(),
            installed: true,
            enabled,
            running,
            system: true,
            error: String::new(),
            icon,
            has_page: self.page.is_some(),
            has_settings: !self.settings_schema.properties.is_empty(),
            services: vec![CatalogService {
                id: "main".into(),
                name: self.name.clone(),
                description: self.description.clone(),
                i18n: self.i18n.clone(),
                unit: self.unit.clone(),
                running,
                enabled,
            }],
            memory_budget_bytes: 0,
            memory_budget_used_bytes: 0,
            memory_budget_total_bytes: 0,
            directories: Vec::new(),
        }
    }

    pub fn entry(&self) -> Result<CatalogEntry, String> {
        let summary = self.summary();
        let settings = self.settings()?;
        let mut schema = self.settings_schema.clone();
        if schema.type_name.is_empty() {
            schema.type_name = "object".into();
        }
        Ok(CatalogEntry {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            i18n: self.i18n.clone(),
            kind: self.kind.clone(),
            author: String::new(),
            homepage: String::new(),
            version: self.version.clone(),
            versions: vec![self.version.clone()],
            installed: true,
            enabled: summary.enabled,
            running: summary.running,
            system: true,
            error: String::new(),
            capabilities: self.capabilities.clone(),
            video: None,
            device_variants: Vec::new(),
            activation_conflicts: Vec::new(),
            services: summary.services,
            icon: self.icon.clone(),
            page: self.page.clone(),
            routes: Vec::new(),
            settings_schema: schema,
            settings,
            secrets_configured: Vec::new(),
            memory_budget_bytes: 0,
            memory_budget_used_bytes: 0,
            memory_budget_total_bytes: 0,
            directories: Vec::new(),
        })
    }

    pub fn settings(&self) -> Result<serde_json::Value, String> {
        let Some(endpoint) = &self.dbus else {
            return Ok(serde_json::json!({}));
        };
        let payload = crate::dbus_client::call_string(
            &endpoint.bus_name,
            &endpoint.object_path,
            &endpoint.interface,
            &endpoint.get_settings,
            None,
            true,
        )?;
        serde_json::from_str(&payload).map_err(|e| format!("decode system plugin {} settings: {e}", self.id))
    }

    pub fn update_settings(&self, update: serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
        let Some(endpoint) = &self.dbus else {
            return Err(format!("system plugin {} has no editable settings", self.id));
        };
        let mut settings = match self.settings()? {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        for (name, value) in update {
            settings.insert(name, value);
        }
        let payload = serde_json::to_string(&settings).map_err(|e| e.to_string())?;
        crate::dbus_client::call_string(
            &endpoint.bus_name,
            &endpoint.object_path,
            &endpoint.interface,
            &endpoint.update_settings,
            Some(&payload),
            false,
        )?;
        Ok(())
    }
}

pub fn load_system_plugins() -> Result<BTreeMap<String, SystemPlugin>, String> {
    let entries = match fs::read_dir(paths::SYSTEM_PLUGINS) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(format!("read system plugin registrations: {err}")),
    };
    let mut result = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
            continue;
        }
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let data = fs::read(entry.path()).map_err(|e| {
            format!(
                "read system plugin registration {}: {e}",
                entry.file_name().to_string_lossy()
            )
        })?;
        let mut registration: SystemPlugin = serde_json::from_slice(&data).map_err(|e| {
            format!(
                "decode system plugin registration {}: {e}",
                entry.file_name().to_string_lossy()
            )
        })?;
        if registration.settings_schema.type_name.is_empty() {
            registration.settings_schema.type_name = "object".into();
        }
        if let Some(page) = &mut registration.page {
            if page.renderer.is_empty() {
                page.renderer = "html".into();
            }
        }
        if !paths::valid_id(&registration.id) {
            return Err(format!("invalid id {:?}", registration.id));
        }
        if result.contains_key(&registration.id) {
            return Err(format!("duplicate system plugin id {:?}", registration.id));
        }
        let _ = Path::new(paths::SYSTEM_PLUGINS);
        result.insert(registration.id.clone(), registration);
    }
    Ok(result)
}
