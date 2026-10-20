use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub fn package_dependencies(fields: &BTreeMap<String, String>) -> Vec<String> {
    fields
        .get("Depends")
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|value| !value.is_empty())
        .collect()
}

pub fn installed_package_dependencies(
    package: &str,
    extension_version: &str,
) -> Option<Vec<String>> {
    dependencies_from_databases(
        Path::new("/var/lib/opkg/info"),
        Path::new(crate::paths::OPKG_SLOTS),
        Path::new(crate::paths::EXTENSION_PACKAGE_METADATA),
        package,
        extension_version,
    )
}

pub fn installed_package_version(package: &str, extension_version: &str) -> Option<String> {
    package_fields_from_databases(
        Path::new("/var/lib/opkg/info"),
        Path::new(crate::paths::OPKG_SLOTS),
        Path::new(crate::paths::EXTENSION_PACKAGE_METADATA),
        package,
        extension_version,
    )?
    .get("Version")
    .cloned()
}

fn dependencies_from_databases(
    active_info: &Path,
    slots: &Path,
    metadata: &Path,
    package: &str,
    extension_version: &str,
) -> Option<Vec<String>> {
    package_fields_from_databases(active_info, slots, metadata, package, extension_version)
        .map(|fields| package_dependencies(&fields))
}

fn package_fields_from_databases(
    active_info: &Path,
    slots: &Path,
    metadata: &Path,
    package: &str,
    extension_version: &str,
) -> Option<BTreeMap<String, String>> {
    // Plugin payloads are shared across boots, while opkg records belong to each OS slot.
    let filename = format!("{package}.control");
    let saved = metadata
        .join(package)
        .join(format!("{extension_version}.control"));
    let fields = matching_package_fields(
        &active_info.join(&filename),
        package,
        Some(extension_version),
    )
    .or_else(|| matching_package_fields(&saved, package, Some(extension_version)))
    .or_else(|| {
        ["A", "B"].into_iter().find_map(|slot| {
            matching_package_fields(
                &slots.join(slot).join("opkg/info").join(&filename),
                package,
                Some(extension_version),
            )
        })
    })?;
    let dependencies = package_dependencies(&fields);
    let record =
        format!(
        "Package: {}\nVersion: {}\nArchitecture: {}\nX-OneKVM-Extension-Version: {}\nDepends: {}\n",
        fields["Package"], fields["Version"], fields["Architecture"], extension_version,
        dependencies.join(", "),
    );
    // Persist only validated package metadata; failure to save must not hide known dependencies.
    if fs::read(&saved).ok().as_deref() != Some(record.as_bytes()) {
        let _ = crate::sys::atomic_write(&saved, record.as_bytes(), 0o600);
    }
    Some(fields)
}

/// Compare opkg/Debian versions, including epoch, numeric runs and package revision.
pub fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    fn parts(value: &str) -> (&str, &str, &str) {
        let (epoch, rest) = value.split_once(':').unwrap_or(("0", value));
        let (version, revision) = rest.rsplit_once('-').unwrap_or((rest, "0"));
        (epoch, version, revision)
    }
    fn order(byte: u8) -> i32 {
        match byte {
            b'~' => -1,
            0 | b'0'..=b'9' => 0,
            b'A'..=b'Z' | b'a'..=b'z' => byte as i32,
            _ => byte as i32 + 256,
        }
    }
    fn compare_part(mut left: &[u8], mut right: &[u8]) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        while !left.is_empty() || !right.is_empty() {
            while left.first().is_some_and(|b| !b.is_ascii_digit())
                || right.first().is_some_and(|b| !b.is_ascii_digit())
            {
                let result = order(left.first().copied().unwrap_or(0))
                    .cmp(&order(right.first().copied().unwrap_or(0)));
                if result != Ordering::Equal {
                    return result;
                }
                left = left.get(1..).unwrap_or_default();
                right = right.get(1..).unwrap_or_default();
            }
            while left.first() == Some(&b'0') {
                left = &left[1..];
            }
            while right.first() == Some(&b'0') {
                right = &right[1..];
            }
            let a = left.iter().take_while(|b| b.is_ascii_digit()).count();
            let b = right.iter().take_while(|b| b.is_ascii_digit()).count();
            let result = a.cmp(&b).then_with(|| left[..a].cmp(&right[..b]));
            if result != Ordering::Equal {
                return result;
            }
            left = &left[a..];
            right = &right[b..];
        }
        Ordering::Equal
    }
    let (ae, av, ar) = parts(left);
    let (be, bv, br) = parts(right);
    compare_part(ae.as_bytes(), be.as_bytes())
        .then_with(|| compare_part(av.as_bytes(), bv.as_bytes()))
        .then_with(|| compare_part(ar.as_bytes(), br.as_bytes()))
}

fn dependencies_from_info(
    info: &Path,
    package: &str,
    extension_version: Option<&str>,
) -> Option<Vec<String>> {
    matching_package_fields(
        &info.join(format!("{package}.control")),
        package,
        extension_version,
    )
    .map(|fields| package_dependencies(&fields))
}

fn matching_package_fields(
    path: &Path,
    package: &str,
    extension_version: Option<&str>,
) -> Option<BTreeMap<String, String>> {
    let data = fs::read(path).ok()?;
    let fields = parse_control(&data).ok()?;
    if fields.get("Package")?.as_str() != package {
        return None;
    }
    if let Some(version) = extension_version {
        if fields.get("X-OneKVM-Extension-Version")?.as_str() != version {
            return None;
        }
    }
    Some(fields)
}

pub fn system_plugin_dependencies(id: &str) -> Option<Vec<String>> {
    let info = Path::new("/var/lib/opkg/info");
    let registration = format!("{}/{id}.json", crate::paths::SYSTEM_PLUGINS);
    dependencies_for_registration(info, &registration)
}

fn dependencies_for_registration(info: &Path, registration: &str) -> Option<Vec<String>> {
    for entry in fs::read_dir(info).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("list") {
            continue;
        }
        let Ok(files) = fs::read_to_string(&path) else {
            continue;
        };
        // opkg may append a tab-separated file mode to each path.
        if files
            .lines()
            .any(|file| file.split_whitespace().next() == Some(registration))
        {
            return dependencies_from_info(info, path.file_stem()?.to_str()?, None);
        }
    }
    None
}

pub fn parse_control(data: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(data).map_err(|_| "control file is not UTF-8".to_string())?;
    let mut fields = BTreeMap::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if current.is_empty() {
                return Err("invalid control continuation".into());
            }
            fields
                .entry(current.clone())
                .and_modify(|value: &mut String| {
                    value.push('\n');
                    value.push_str(line.trim());
                });
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("invalid control line {line:?}"));
        };
        current = key.to_owned();
        fields.insert(current.clone(), value.trim().to_owned());
    }
    for required in ["Package", "Version", "Architecture"] {
        if fields
            .get(required)
            .map(String::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            return Err(format!("control field {required} is missing"));
        }
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::{
        dependencies_for_registration, dependencies_from_databases, dependencies_from_info,
        package_dependencies, parse_control,
    };

    #[test]
    fn opkg_version_order_handles_revisions_epochs_and_prereleases() {
        use std::cmp::Ordering;
        for (older, newer) in [
            ("1.9.9-r9", "1.10.0-r0"),
            ("1.0-r2", "1.0-r10"),
            ("1.0~rc1-r0", "1.0-r0"),
            ("9.9-r0", "1:1.0-r0"),
            ("1.0-r1.debug20261013", "1.0-r1.debug20261014"),
            ("1.0", "1.0+git1"),
        ] {
            assert_eq!(
                super::compare_versions(older, newer),
                Ordering::Less,
                "{older} < {newer}"
            );
            assert_eq!(super::compare_versions(newer, older), Ordering::Greater);
        }
        for (a, b) in [
            ("1.0", "1.0-r0"),
            ("1.01-r01", "1.1-r1"),
            ("1.0", "0:1.0-0"),
        ] {
            // r0 is an OE revision, distinct from the absent Debian revision 0.
            let expected = if b.ends_with("-r0") {
                Ordering::Less
            } else {
                Ordering::Equal
            };
            assert_eq!(super::compare_versions(a, b), expected);
        }
    }

    fn temporary_directory() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "onekvm-dependencies-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn unavailable_package_metadata_is_distinct_from_no_dependencies() {
        let root = temporary_directory();
        assert_eq!(dependencies_from_info(&root, "plugin", None), None);
        let control = root.join("plugin.control");
        std::fs::write(
            &control,
            b"Package: plugin\nVersion: 1\nArchitecture: all\n",
        )
        .unwrap();
        assert_eq!(
            dependencies_from_info(&root, "plugin", None),
            Some(Vec::new())
        );
        assert_eq!(dependencies_from_info(&root, "plugin", Some("1")), None);
        std::fs::write(&control, b"Package: plugin\nVersion: 1-r1\nArchitecture: all\nX-OneKVM-Extension-Version: 1\nDepends: musl\n").unwrap();
        assert_eq!(
            dependencies_from_info(&root, "plugin", Some("1")),
            Some(vec!["musl".into()])
        );
        assert_eq!(dependencies_from_info(&root, "plugin", Some("2")), None);
        std::fs::write(
            root.join("plugin.list"),
            b"/usr/share/onekvm/system-plugins/test.json\t0100644\n",
        )
        .unwrap();
        assert_eq!(
            dependencies_for_registration(&root, "/usr/share/onekvm/system-plugins/test.json"),
            Some(vec!["musl".into()])
        );
        std::fs::write(&control, b"Package: other\nVersion: 1\nArchitecture: all\n").unwrap();
        assert_eq!(dependencies_from_info(&root, "plugin", None), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_plugin_dependencies_follow_its_version_across_os_slots() {
        let root = temporary_directory();
        let active = root.join("active/info");
        let slots = root.join("slots");
        let metadata = root.join("metadata");
        let previous = slots.join("A/opkg/info");
        let other = slots.join("B/opkg/info");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::create_dir_all(&previous).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let dependencies =
            || dependencies_from_databases(&active, &slots, &metadata, "plugin", "2");
        assert_eq!(dependencies(), None);
        std::fs::write(active.join("plugin.control"), b"Package: plugin\nVersion: 1-r0\nArchitecture: all\nX-OneKVM-Extension-Version: 1\nDepends: obsolete\n").unwrap();
        std::fs::write(previous.join("plugin.control"), b"Package: plugin\nVersion: 3-r0\nArchitecture: all\nX-OneKVM-Extension-Version: 3\nDepends: future\n").unwrap();
        assert_eq!(dependencies(), None);
        std::fs::write(previous.join("plugin.control"), b"Package: plugin\nVersion: 2-r0\nArchitecture: all\nX-OneKVM-Extension-Version: 2\nDepends: runtime (>= 1), musl\n").unwrap();
        assert_eq!(
            dependencies(),
            Some(vec!["runtime (>= 1)".into(), "musl".into()])
        );
        std::fs::write(active.join("plugin.control"), b"Package: plugin\nVersion: 2-r1\nArchitecture: all\nX-OneKVM-Extension-Version: 2\nDepends: updated-runtime\n").unwrap();
        assert_eq!(dependencies(), Some(vec!["updated-runtime".into()]));
        std::fs::write(
            active.join("plugin.control"),
            b"Package: plugin\nVersion: 2-r2\nArchitecture: all\nX-OneKVM-Extension-Version: 2\n",
        )
        .unwrap();
        assert_eq!(dependencies(), Some(Vec::new()));
        std::fs::remove_file(active.join("plugin.control")).unwrap();
        std::fs::remove_dir_all(&metadata).unwrap();
        std::fs::rename(
            previous.join("plugin.control"),
            other.join("plugin.control"),
        )
        .unwrap();
        assert_eq!(
            dependencies(),
            Some(vec!["runtime (>= 1)".into(), "musl".into()])
        );
        std::fs::remove_dir_all(&slots).unwrap();
        assert_eq!(
            dependencies(),
            Some(vec!["runtime (>= 1)".into(), "musl".into()])
        );
        assert_eq!(
            dependencies_from_databases(&active, &slots, &metadata, "plugin", "3"),
            None
        );
        std::fs::write(metadata.join("plugin/2.control"), b"Package: other\nVersion: 2-r0\nArchitecture: all\nX-OneKVM-Extension-Version: 2\nDepends: unrelated\n").unwrap();
        assert_eq!(dependencies(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_metadata_preserves_versions_alternatives_and_continuations() {
        let fields = parse_control(b"Package: plugin\nVersion: 1\nArchitecture: all\nDepends: musl (>= 1.2.6),\n libfoo | libbar, kernel-module-tun (= 6.18)\n").unwrap();
        assert_eq!(
            package_dependencies(&fields),
            [
                "musl (>= 1.2.6)",
                "libfoo | libbar",
                "kernel-module-tun (= 6.18)"
            ]
        );
        let fields = parse_control(b"Package: plugin\nVersion: 1\nArchitecture: all\n").unwrap();
        assert!(package_dependencies(&fields).is_empty());
    }

    #[test]
    fn parses_required_fields() {
        let fields = parse_control(
            b"Package: onekvm-extension-edid\nVersion: 1.0.0\nArchitecture: riscv64\n",
        )
        .unwrap();
        assert_eq!(fields["Package"], "onekvm-extension-edid");
        assert_eq!(fields["Version"], "1.0.0");
    }

    #[test]
    fn rejects_missing_package() {
        assert!(parse_control(b"Version: 1\nArchitecture: all\n").is_err());
    }
}
