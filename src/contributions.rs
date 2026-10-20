//! Generic, manifest-backed extension point registry. Contributions are resolved
//! from active plugins on each query so install, activate, disable and removal
//! need no separate registration cleanup.

use std::collections::BTreeMap;
use std::fs;

use serde::Serialize;
use serde_json::Value;

use crate::manifest::{load_installed, Contribution, ExtensionPoint};
use crate::paths;
use crate::sandbox;
use crate::state::read_state;
use crate::system_plugin::load_system_plugins;

#[derive(Serialize)]
pub struct ResolvedContribution {
    pub point: String,
    pub id: String,
    pub source: String,
    pub version: u32,
    pub generation: u64,
    pub permissions: Vec<String>,
    pub data: Value,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub method: String,
    /// Installed extension package root. Consumers that load package-owned
    /// assets (such as screen-display apps) may resolve a safe relative path
    /// below this directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_root: Option<String>,
}

pub fn list(point_id: &str) -> Result<Vec<ResolvedContribution>, String> {
    if !valid_point(point_id) {
        return Err(format!("invalid extension point {point_id:?}"));
    }
    let mut points: BTreeMap<String, u32> = BTreeMap::new();
    let mut sources: Vec<(String, Vec<Contribution>, Option<String>, u64, Vec<String>)> =
        Vec::new();
    for system in load_system_plugins()?.into_values() {
        if !sandbox::is_enabled(&system.unit) {
            continue;
        }
        add_points(&mut points, &system.id, &system.extension_points);
        sources.push((system.id, system.contributions, None, 0, Vec::new()));
    }
    if let Ok(entries) = fs::read_dir(paths::APPS_ROOT) {
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) || !paths::valid_id(&id) {
                continue;
            }
            if crate::transaction::pending(&id) {
                continue;
            }
            let Ok(state) = read_state(&id) else { continue };
            if !state.enabled {
                continue;
            }
            let Ok(installed) = load_installed(&id, &state.active_version) else {
                continue;
            };
            add_points(&mut points, &id, &installed.manifest.extension_points);
            sources.push((
                id,
                installed.manifest.contributions,
                Some(installed.root.to_string_lossy().into_owned()),
                state.lifecycle_revision,
                installed.manifest.permissions.clone(),
            ));
        }
    }
    let Some(version) = points.get(point_id) else {
        return Ok(Vec::new());
    };
    Ok(resolve(point_id, *version, sources))
}

fn resolve(
    point_id: &str,
    version: u32,
    sources: Vec<(String, Vec<Contribution>, Option<String>, u64, Vec<String>)>,
) -> Vec<ResolvedContribution> {
    let mut result = Vec::new();
    for (source, contributions, package_root, generation, permissions) in sources {
        for item in contributions {
            if item.point != point_id || item.version != version || !paths::valid_id(&item.id) {
                continue;
            }
            result.push(ResolvedContribution {
                point: item.point,
                id: format!("{source}/{}", item.id),
                source: source.clone(),
                version: item.version,
                generation,
                permissions: permissions.clone(),
                data: item.data,
                method: item.method,
                package_root: package_root.clone(),
            });
        }
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contributions_are_namespaced_and_versioned() {
        let item = |version| Contribution {
            point: "screen-display.widget".into(),
            id: "temperature".into(),
            version,
            data: serde_json::json!({"title":"Temperature"}),
            method: String::new(),
        };
        let result = resolve(
            "screen-display.widget",
            1,
            vec![
                (
                    "weather".into(),
                    vec![item(1), item(2)],
                    None,
                    7,
                    Vec::new(),
                ),
                ("thermal".into(), vec![item(1)], None, 3, Vec::new()),
            ],
        );
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "thermal/temperature");
        assert_eq!(result[1].id, "weather/temperature");
    }
}

fn add_points(points: &mut BTreeMap<String, u32>, owner: &str, declared: &[ExtensionPoint]) {
    for point in declared {
        if point.id.starts_with(&format!("{owner}.")) && valid_point(&point.id) && point.version > 0
        {
            points.insert(point.id.clone(), point.version);
        }
    }
}

fn valid_point(value: &str) -> bool {
    let Some((owner, name)) = value.split_once('.') else {
        return false;
    };
    paths::valid_id(owner) && paths::valid_id(name)
}

/// Generic change token for every contribution consumer, atomically published
/// after state mutations. Consumers may watch its parent with inotify; polling
/// also works across host restarts. Never contains extension settings.
pub fn changed() -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    crate::sys::atomic_write(
        std::path::Path::new(paths::CONTRIBUTIONS_GENERATION),
        format!("{now}-{}\n", std::process::id()).as_bytes(),
        0o644,
    )
}
