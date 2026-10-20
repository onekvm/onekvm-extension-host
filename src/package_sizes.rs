//! Marketplace estimates follow the packages opkg would install, including recommendations.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::control::{compare_versions, parse_control};

#[derive(Clone)]
struct Dependency {
    name: String,
    constraint: Option<(String, String)>,
}

fn dependency(value: &str) -> Dependency {
    let (name, constraint) = value.trim().split_once('(').unwrap_or((value.trim(), ""));
    let mut parts = constraint.trim_end_matches(')').split_whitespace();
    Dependency {
        name: name.trim().to_owned(),
        constraint: parts
            .next()
            .zip(parts.next())
            .map(|(op, version)| (op.into(), version.into())),
    }
}

fn dependencies(value: &str) -> Vec<Vec<Dependency>> {
    value
        .split(',')
        .map(|group| {
            group
                .split('|')
                .map(dependency)
                .filter(|dep| !dep.name.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|group| !group.is_empty())
        .collect()
}

fn satisfies(version: &str, constraint: &Option<(String, String)>) -> bool {
    let Some((op, wanted)) = constraint else {
        return true;
    };
    let order = compare_versions(version, wanted);
    match op.as_str() {
        "=" | "==" => order.is_eq(),
        ">=" => !order.is_lt(),
        "<=" => !order.is_gt(),
        ">>" | ">" => order.is_gt(),
        "<<" | "<" => order.is_lt(),
        _ => false,
    }
}

pub struct Package {
    pub fields: BTreeMap<String, String>,
    priority: i32,
    dependencies: Vec<Vec<Dependency>>,
    provides: Vec<Dependency>,
}

impl Package {
    fn new(fields: BTreeMap<String, String>, priority: i32) -> Self {
        let dependencies = ["Pre-Depends", "Depends", "Recommends"]
            .into_iter()
            .flat_map(|key| dependencies(fields.get(key).map(String::as_str).unwrap_or("")))
            .collect();
        let provides = fields
            .get("Provides")
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(dependency)
            .collect();
        Self {
            fields,
            priority,
            dependencies,
            provides,
        }
    }

    fn field(&self, key: &str) -> &str {
        self.fields.get(key).map(String::as_str).unwrap_or("")
    }

    fn size(&self, key: &str) -> u64 {
        self.field(key).parse().unwrap_or(0)
    }

    fn matches(&self, dep: &Dependency) -> bool {
        (self.field("Package") == dep.name && satisfies(self.field("Version"), &dep.constraint))
            || self.provides.iter().any(|provided| {
                provided.name == dep.name
                    && (dep.constraint.is_none()
                        || provided
                            .constraint
                            .as_ref()
                            .is_some_and(|(_, version)| satisfies(version, &dep.constraint)))
            })
    }
}

pub struct PackageIndex {
    packages: BTreeMap<String, Vec<Package>>,
    installed: Vec<Package>,
}

impl PackageIndex {
    pub fn load(lists_root: &Path, lists: &[String]) -> Result<Self, String> {
        let output = crate::sys::opkg_output(&["print-architecture"])?;
        let architectures = String::from_utf8_lossy(&output)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                if parts.next()? != "arch" {
                    return None;
                }
                Some((parts.next()?.to_owned(), parts.next()?.parse::<i32>().ok()?))
            })
            .collect::<BTreeMap<_, _>>();
        if architectures.is_empty() {
            return Err("opkg returned no accepted architectures".into());
        }
        let lists = lists
            .iter()
            .filter_map(|list| fs::read_to_string(lists_root.join(list)).ok())
            .collect::<Vec<_>>();
        let status = fs::read_to_string("/var/lib/opkg/status").unwrap_or_default();
        Ok(Self::from_controls(&lists, &status, &architectures))
    }

    fn from_controls(
        lists: &[String],
        status: &str,
        architectures: &BTreeMap<String, i32>,
    ) -> Self {
        let mut packages: BTreeMap<String, Vec<Package>> = BTreeMap::new();
        for text in lists {
            for paragraph in text.split("\n\n") {
                let Ok(fields) = parse_control(paragraph.as_bytes()) else {
                    continue;
                };
                let Some(name) = fields.get("Package") else {
                    continue;
                };
                let Some(priority) = fields
                    .get("Architecture")
                    .and_then(|arch| architectures.get(arch))
                else {
                    continue;
                };
                packages
                    .entry(name.clone())
                    .or_default()
                    .push(Package::new(fields, *priority));
            }
        }
        for candidates in packages.values_mut() {
            candidates.sort_by(|a, b| {
                b.priority
                    .cmp(&a.priority)
                    .then_with(|| compare_versions(b.field("Version"), a.field("Version")))
            });
        }
        let installed = status
            .split("\n\n")
            .filter_map(|paragraph| parse_control(paragraph.as_bytes()).ok())
            .filter(|fields| {
                fields
                    .get("Status")
                    .is_some_and(|status| status.split_whitespace().last() == Some("installed"))
            })
            .map(|fields| Package::new(fields, 0))
            .collect();
        Self {
            packages,
            installed,
        }
    }

    pub fn extensions(&self) -> impl Iterator<Item = &Package> {
        self.packages
            .values()
            .filter_map(|candidates| candidates.first())
            .filter(|package| package.fields.contains_key("X-OneKVM-Extension-Manifest"))
    }

    fn candidate(&self, dep: &Dependency) -> Option<&Package> {
        // Prefer a real package name, then the best compatible virtual provider.
        self.packages
            .get(&dep.name)
            .and_then(|candidates| candidates.iter().find(|package| package.matches(dep)))
            .or_else(|| {
                self.packages
                    .values()
                    .flatten()
                    .filter(|package| package.matches(dep))
                    .max_by(|a, b| {
                        a.priority
                            .cmp(&b.priority)
                            .then_with(|| compare_versions(a.field("Version"), b.field("Version")))
                    })
            })
    }

    pub fn sizes(&self, root: &Package) -> (u64, u64) {
        let mut selected = BTreeMap::from([(root.field("Package"), root)]);
        let mut visited = BTreeSet::new();
        let mut pending = vec![root];
        let (mut download, mut unpacked) = (0u64, 0u64);
        while let Some(package) = pending.pop() {
            if !visited.insert(package.field("Package")) {
                continue;
            }
            let installed = self.installed.iter().any(|current| {
                current.field("Package") == package.field("Package")
                    && !compare_versions(current.field("Version"), package.field("Version")).is_lt()
            });
            // The root's installed footprint remains useful on an already installed card.
            // Shared dependencies already provided by this OS require no extra storage.
            if !installed || std::ptr::eq(package, root) {
                let installed_size = package.size("Installed-Size");
                unpacked = unpacked.saturating_add(if installed_size > 0 {
                    installed_size
                } else {
                    package.size("Size")
                });
            }
            if installed {
                continue;
            }
            download = download.saturating_add(package.size("Size"));
            for group in &package.dependencies {
                if group.iter().any(|dep| {
                    self.installed
                        .iter()
                        .chain(selected.values().copied())
                        .any(|current| current.matches(dep))
                }) {
                    continue;
                }
                if let Some(candidate) = group.iter().find_map(|dep| self.candidate(dep)) {
                    selected.insert(candidate.field("Package"), candidate);
                    pending.push(candidate);
                }
            }
        }
        (download, unpacked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(text: &str, status: &str) -> PackageIndex {
        PackageIndex::from_controls(
            &[text.into()],
            status,
            &BTreeMap::from([
                ("all".into(), 1),
                ("aarch64".into(), 20),
                ("pro".into(), 40),
            ]),
        )
    }

    fn package(name: &str, extra: &str) -> String {
        format!("Package: {name}\nVersion: 1-r0\nArchitecture: aarch64\nSize: 100\nInstalled-Size: 300\n{extra}\n\n")
    }

    #[test]
    fn recommendations_transitive_libraries_and_shared_dependencies_are_counted_once() {
        let text = package("plugin", "Depends: host\nRecommends: tool, module")
            + &package("tool", "Depends: libextra, module")
            + &package("module", "Depends: libextra")
            + &package("libextra", "Depends: tool"); // cycle
        let idx = index(
            &text,
            "Package: host\nVersion: 1-r0\nArchitecture: aarch64\nStatus: install ok installed\n",
        );
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (400, 1200));
    }

    #[test]
    fn installed_dependencies_are_excluded_but_required_upgrades_are_included() {
        let text = package("plugin", "Depends: libsame, libold (>= 1-r0)")
            + &package("libsame", "")
            + &package("libold", "");
        let idx = index(&text, "Package: libsame\nVersion: 1-r0\nArchitecture: aarch64\nStatus: install ok installed\n\nPackage: libold\nVersion: 0-r0\nArchitecture: aarch64\nStatus: install ok installed\n");
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (200, 600));
    }

    #[test]
    fn alternatives_and_virtual_providers_respect_versions() {
        let text = package("plugin", "Depends: absent | virtual (= 2), other")
            + &package("provider", "Provides: virtual (= 2), other");
        let idx = index(&text, "");
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (200, 600));
        let idx = index(&text, "Package: provider\nVersion: 1-r0\nProvides: virtual (= 2), other\nArchitecture: aarch64\nStatus: install ok installed\n");
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (100, 300));
    }

    #[test]
    fn candidate_architecture_and_pinned_version_match_package_selection() {
        let text = package("plugin", "Depends: tool (= 1-r0)")
            + &package("tool", "")
                .replace("aarch64", "riscv64")
                .replace("Size: 100", "Size: 9000")
            + &package("tool", "").replace("Size: 100", "Size: 200")
            + &package("tool", "").replace("1-r0", "2-r0");
        let idx = index(&text, "");
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (300, 600));
    }

    #[test]
    fn removed_and_unpacked_packages_do_not_satisfy_dependencies() {
        let text = package("plugin", "Depends: tool") + &package("tool", "");
        let idx = index(
            &text,
            "Package: tool\nVersion: 1-r0\nArchitecture: aarch64\nStatus: deinstall ok config-files\n",
        );
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (200, 600));
    }

    #[test]
    fn installed_root_keeps_its_footprint_without_another_download() {
        let text = package("plugin", "Recommends: tool") + &package("tool", "");
        let status = package("plugin", "Status: install ok installed");
        let idx = index(&text, &status);
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (0, 300));
    }

    #[test]
    fn architecture_priority_is_applied_before_size_estimation() {
        let text = package("plugin", "Depends: tool")
            + &package("tool", "")
            + &package("tool", "")
                .replace("aarch64", "pro")
                .replace("Size: 100", "Size: 250");
        let idx = index(&text, "");
        assert_eq!(idx.sizes(&idx.packages["plugin"][0]), (350, 600));
    }
}
