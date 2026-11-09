use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::catalog::{CatalogContribution, CatalogEntry, CatalogService, CatalogSummary};
use crate::manifest::{
    Contribution, ExtensionPage, ExtensionPoint, ExtensionRoute, PageIcon, SettingsSchema,
    Translation,
};
use crate::paths;
use crate::sandbox;

#[derive(Debug, Clone, Deserialize)]
pub struct SystemPlugin {
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub kernel_modules: Vec<String>,
    #[serde(default = "default_system")]
    pub system: bool,
    #[serde(default)]
    pub settings_command: Vec<String>,
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
    pub extension_points: Vec<ExtensionPoint>,
    #[serde(default)]
    pub contributions: Vec<Contribution>,
    #[serde(default)]
    pub icon: Option<PageIcon>,
    #[serde(default)]
    pub page: Option<ExtensionPage>,
    pub unit: String,
    #[serde(default)]
    pub settings_schema: SettingsSchema,
    #[serde(default)]
    pub dbus: Option<SystemPluginDBus>,
    #[serde(default)]
    pub routes: Vec<ExtensionRoute>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemPluginDBus {
    pub bus_name: String,
    pub object_path: String,
    pub interface: String,
    pub get_settings: String,
    pub update_settings: String,
    #[serde(default)]
    pub methods: BTreeMap<String, String>,
}

fn default_system() -> bool {
    true
}

impl SystemPlugin {
    pub fn invoke(&self, method: &str, payload: &str) -> Result<String, String> {
        if !self.system && !sandbox::is_enabled(&self.unit) {
            return Err(format!("plugin {} is disabled", self.id));
        }
        let endpoint = self
            .dbus
            .as_ref()
            .ok_or("system plugin has no D-Bus endpoint")?;
        let member = endpoint.methods.get(method).ok_or_else(|| {
            format!(
                "system plugin {} does not export method {method:?}",
                self.id
            )
        })?;
        crate::dbus_client::call_string(
            &endpoint.bus_name,
            &endpoint.object_path,
            &endpoint.interface,
            member,
            Some(payload),
            true,
        )
    }

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
            system: self.system,
            removable: Some(false),
            error: String::new(),
            icon,
            icon_data_url: String::new(),
            has_page: self.page.is_some(),
            has_toolbar: false,
            toolbar: None,
            has_shell: false,
            shell: None,
            console: None,
            toolbox: None,
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
            auth_provider: None,
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
            permissions: self.permissions.clone(),
            kernel_modules: self.kernel_modules.clone(),
            package_dependencies: crate::control::system_plugin_dependencies(&self.id),
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
            system: self.system,
            removable: Some(false),
            error: String::new(),
            capabilities: self.capabilities.clone(),
            contributions: self.contributions.iter().map(CatalogContribution::from).collect(),
            video: None,
            device_variants: Vec::new(),
            activation_conflicts: Vec::new(),
            services: summary.services,
            icon: self.icon.clone(),
            page: self.page.clone(),
            toolbar: None,
            shell: None,
            console: None,
            toolbox: None,
            routes: self.routes.clone(),
            auth_provider: None,
            settings_schema: schema,
            settings,
            secrets_configured: Vec::new(),
            memory_budget_bytes: 0,
            memory_budget_used_bytes: 0,
            memory_budget_total_bytes: 0,
            directories: Vec::new(),
        })
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        if self.system {
            return Err(format!(
                "system plugin {} cannot be enabled or disabled",
                self.id
            ));
        }
        if enabled {
            crate::contributions::validate_provider(&self.id, &self.extension_points)?;
            let was_enabled = sandbox::is_enabled(&self.unit);
            sandbox::unit_action("unmask", &self.unit)?;
            let result = sandbox::unit_action("enable", &self.unit)
                .and_then(|_| sandbox::unit_action("start", &self.unit));
            if result.is_err() && !was_enabled {
                sandbox::unit_action("mask", &self.unit)?;
                sandbox::unit_action("stop", &self.unit)?;
            }
            result?;
        } else {
            // D-Bus activation and Core's Wants must not revive a disabled provider.
            let was_enabled = sandbox::is_enabled(&self.unit);
            sandbox::unit_action("mask", &self.unit)?;
            if let Err(error) = sandbox::unit_action("stop", &self.unit) {
                if was_enabled {
                    sandbox::unit_action("unmask", &self.unit)?;
                }
                return Err(error);
            }
        }
        crate::contributions::changed()
    }

    fn settings_command(&self, payload: Option<&str>) -> Result<serde_json::Value, String> {
        let (program, args) = self
            .settings_command
            .split_first()
            .ok_or("plugin has no offline settings command")?;
        if !Path::new(program).is_absolute() {
            return Err("plugin settings command must use an absolute executable path".into());
        }
        let mut child = Command::new(program)
            .args(args)
            .stdin(if payload.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("plugin {} settings: {err}", self.id))?;
        if let Some(payload) = payload {
            let result = child
                .stdin
                .take()
                .expect("settings stdin")
                .write_all(payload.as_bytes());
            if let Err(error) = result {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("write plugin settings: {error}"));
            }
        }
        let output = child.wait_with_output().map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "plugin {} settings: {}",
                self.id,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("decode plugin {} settings: {err}", self.id))
    }

    pub fn settings(&self) -> Result<serde_json::Value, String> {
        if !self.settings_command.is_empty() && !sandbox::is_active(&self.unit) {
            return self.settings_command(None);
        }
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
        serde_json::from_str(&payload)
            .map_err(|e| format!("decode system plugin {} settings: {e}", self.id))
    }

    pub fn update_settings(
        &self,
        update: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), String> {
        let Some(endpoint) = &self.dbus else {
            return Err(format!(
                "system plugin {} has no editable settings",
                self.id
            ));
        };
        let mut settings = match self.settings()? {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        for (name, value) in update {
            settings.insert(name, value);
        }
        let payload = serde_json::to_string(&settings).map_err(|e| e.to_string())?;
        if !self.settings_command.is_empty() && !sandbox::is_active(&self.unit) {
            self.settings_command(Some(&payload))?;
            return Ok(());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_plugins_keep_system_protection_unless_explicitly_opted_out() {
        let value = serde_json::json!({"id":"native-test","name":"Native","kind":"device","version":"1.0.0","unit":"onekvm-native-test.service"});
        let protected: SystemPlugin = serde_json::from_value(value.clone()).unwrap();
        assert!(protected.system);
        assert!(protected
            .set_enabled(false)
            .unwrap_err()
            .contains("cannot be enabled or disabled"));
        let mut value = value;
        value["system"] = serde_json::json!(false);
        let optional: SystemPlugin = serde_json::from_value(value).unwrap();
        let summary = optional.summary();
        assert!(!summary.system);
        assert_eq!(summary.removable, Some(false));
    }

    #[test]
    fn offline_settings_commands_read_and_update_without_dbus_activation() {
        let value = serde_json::json!({"id":"native-test","name":"Native","kind":"device","system":false,"version":"1.0.0","unit":"onekvm-native-test.service","settings_command":["/bin/echo","{\"use_default\":true}"]});
        let mut plugin: SystemPlugin = serde_json::from_value(value).unwrap();
        assert_eq!(
            plugin.settings().unwrap(),
            serde_json::json!({"use_default":true})
        );
        plugin.settings_command = vec!["/bin/cat".into()];
        assert_eq!(
            plugin
                .settings_command(Some("{\"use_default\":false}"))
                .unwrap(),
            serde_json::json!({"use_default":false})
        );
        plugin.settings_command = vec!["relative-binary".into()];
        assert!(plugin
            .settings_command(None)
            .unwrap_err()
            .contains("absolute"));
    }

    #[test]
    fn system_plugins_publish_session_routes_without_changing_legacy_registrations() {
        let mut value = serde_json::json!({"id":"screen-display", "name":"Screen", "kind":"device", "version":"0.1.0", "unit":"screen.service"});
        let legacy: SystemPlugin = serde_json::from_value(value.clone()).unwrap();
        assert!(legacy.routes.is_empty());
        value["routes"] = serde_json::json!([{"path":"remote","backend":"unix","socket":"remote.sock","auth":"session"}]);
        let screen: SystemPlugin = serde_json::from_value(value).unwrap();
        assert_eq!(screen.routes[0].socket, "remote.sock");
        assert_eq!(screen.routes[0].auth, "session");
    }
}
