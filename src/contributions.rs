//! Generic, manifest-backed extension point registry. Contributions are resolved
//! from active plugins on each query so install, activate, disable and removal
//! need no separate registration cleanup.

use std::collections::{BTreeMap, BTreeSet};
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
    pub provider: String,
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

#[derive(Clone, Serialize)]
pub struct ResolvedPoint {
    pub id: String,
    pub provider: String,
    pub version: u32,
    pub aliases: Vec<String>,
}

type Source = (String, Vec<Contribution>, Option<String>, u64, Vec<String>);
type Registry = (BTreeMap<String, ResolvedPoint>, Vec<Source>);

pub fn points() -> Result<Vec<ResolvedPoint>, String> {
    let (points, _) = registry(None)?;
    Ok(points
        .into_iter()
        .filter_map(|(name, point)| (name == point.id).then_some(point))
        .collect())
}

pub fn list(point_id: &str) -> Result<Vec<ResolvedContribution>, String> {
    if !valid_point(point_id) {
        return Err(format!("invalid extension point {point_id:?}"));
    }
    let (points, sources) = registry(None)?;
    let Some(point) = points.get(point_id) else {
        return Ok(Vec::new());
    };
    Ok(resolve(point_id, point, sources))
}

/// Called under the lifecycle lock before enabling or activating a provider.
/// Exclude its old registration so upgrades can retain their names and aliases.
pub fn validate_provider(owner: &str, declared: &[ExtensionPoint]) -> Result<(), String> {
    if declared.is_empty() {
        return Ok(());
    }
    let (mut points, _) = registry(Some(owner))?;
    add_points(&mut points, owner, declared)
}

fn registry(exclude_provider: Option<&str>) -> Result<Registry, String> {
    let mut points = BTreeMap::new();
    let mut sources = Vec::new();
    for system in load_system_plugins()?.into_values() {
        if exclude_provider == Some(system.id.as_str()) || !sandbox::is_enabled(&system.unit) {
            continue;
        }
        add_points(&mut points, &system.id, &system.extension_points)?;
        sources.push((system.id, system.contributions, None, 0, Vec::new()));
    }
    if let Ok(entries) = fs::read_dir(paths::APPS_ROOT) {
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) || !paths::valid_id(&id) {
                continue;
            }
            if exclude_provider == Some(id.as_str()) || crate::transaction::pending(&id) {
                continue;
            }
            let Ok(state) = read_state(&id) else { continue };
            if !state.enabled {
                continue;
            }
            let Ok(installed) = load_installed(&id, &state.active_version) else {
                continue;
            };
            add_points(&mut points, &id, &installed.manifest.extension_points)?;
            sources.push((
                id,
                installed.manifest.contributions,
                Some(installed.root.to_string_lossy().into_owned()),
                state.lifecycle_revision,
                installed.manifest.permissions.clone(),
            ));
        }
    }
    Ok((points, sources))
}

fn resolve(
    point_id: &str,
    point: &ResolvedPoint,
    sources: Vec<Source>,
) -> Vec<ResolvedContribution> {
    let mut result = Vec::new();
    for (source, mut contributions, package_root, generation, permissions) in sources {
        // During migration a source can declare both names. Prefer its canonical
        // declaration and publish each local ID once through either query name.
        contributions.sort_by_key(|item| item.point != point.id);
        let mut seen = BTreeSet::new();
        for item in contributions {
            if (item.point != point.id && !point.aliases.contains(&item.point))
                || item.version != point.version
                || !paths::valid_id(&item.id)
                || !seen.insert(item.id.clone())
            {
                continue;
            }
            result.push(ResolvedContribution {
                provider: point.provider.clone(),
                point: point_id.to_owned(),
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
    fn registry_tracks_provider_and_rejects_primary_and_alias_collisions() {
        let point = ExtensionPoint {
            id: "extend.mcp".into(),
            version: 1,
            aliases: vec!["mcp.tool".into()],
        };
        let mut registry = BTreeMap::new();
        add_points(&mut registry, "mcp", &[point]).unwrap();
        assert_eq!(registry["extend.mcp"].provider, "mcp");
        assert_eq!(registry["mcp.tool"].id, "extend.mcp");
        for name in ["extend.mcp", "mcp.tool"] {
            let mut copy = registry.clone();
            assert!(add_points(
                &mut copy,
                "impostor",
                &[ExtensionPoint {
                    id: name.into(),
                    version: 1,
                    aliases: vec![]
                }]
            )
            .is_err());
        }
        add_points(
            &mut registry,
            "mqtt",
            &[ExtensionPoint {
                id: "extend.mqtt".into(),
                version: 1,
                aliases: vec![],
            }],
        )
        .unwrap();
        assert_eq!(registry["extend.mqtt"].provider, "mqtt");
    }

    #[test]
    fn aliases_merge_legacy_contributors_and_prefer_canonical_declarations() {
        let point = ResolvedPoint {
            id: "extend.mcp".into(),
            provider: "mcp".into(),
            version: 1,
            aliases: vec!["mcp.tool".into()],
        };
        let item = |point: &str, method: &str| Contribution {
            point: point.into(),
            id: "status".into(),
            version: 1,
            data: serde_json::json!({}),
            method: method.into(),
        };
        for query in ["extend.mcp", "mcp.tool"] {
            let sources = vec![
                (
                    "old".into(),
                    vec![item("mcp.tool", "legacy")],
                    None,
                    7,
                    vec![],
                ),
                (
                    "new".into(),
                    vec![item("mcp.tool", "legacy"), item("extend.mcp", "current")],
                    None,
                    9,
                    vec![],
                ),
            ];
            let result = resolve(query, &point, sources);
            assert_eq!(result.len(), 2);
            assert_eq!(result[0].id, "new/status");
            assert_eq!(result[0].method, "current");
            assert_eq!(result[1].method, "legacy");
            assert!(result
                .iter()
                .all(|entry| entry.point == query && entry.provider == "mcp"));
        }
    }

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
            &ResolvedPoint {
                id: "screen-display.widget".into(),
                provider: "screen-display".into(),
                version: 1,
                aliases: Vec::new(),
            },
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

fn add_points(
    points: &mut BTreeMap<String, ResolvedPoint>,
    owner: &str,
    declared: &[ExtensionPoint],
) -> Result<(), String> {
    for point in declared {
        let resolved = ResolvedPoint {
            id: point.id.clone(),
            provider: owner.into(),
            version: point.version,
            aliases: point.aliases.clone(),
        };
        for name in std::iter::once(&point.id).chain(&point.aliases) {
            if !valid_point(name) || point.version == 0 {
                return Err(format!("invalid extension point {name:?} from {owner:?}"));
            }
            if let Some(previous) = points.get(name) {
                return Err(format!(
                    "extension point {name:?} conflicts between {:?} and {owner:?}",
                    previous.provider
                ));
            }
            points.insert(name.clone(), resolved.clone());
        }
    }
    Ok(())
}

fn valid_point(value: &str) -> bool {
    let Some((owner, name)) = value.split_once('.') else {
        return false;
    };
    paths::valid_id(owner) && paths::valid_id(name)
}

/// Generic change token for every contribution consumer, atomically published
/// after state mutations. Consumers watch its parent with inotify and reconcile on
/// startup/reattachment, including after host restarts. Never contains extension settings.
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
