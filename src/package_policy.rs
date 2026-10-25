//! Extension transactions must not replace packages supplied by the system image.
//! The immutable image baseline is authoritative, including shared libraries. New
//! optional runtimes remain installable; installed extensions remain updateable.

use std::collections::BTreeSet;
use std::fs;

use crate::control::parse_control;

const BASELINE: &str = "/usr/share/onekvm/opkg-baseline/status";
pub const UPDATE_HINT: &str = "System-image packages cannot be changed by plugin installation; update the signed sqfs system image if newer system dependencies are required";

fn protected_packages(status: &str) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let mut installed = 0;
    for stanza in status.split("\n\n").filter(|part| !part.trim().is_empty()) {
        let fields = parse_control(stanza.as_bytes())
            .map_err(|error| format!("invalid system-image package baseline: {error}"))?;
        if !fields
            .get("Status")
            .is_some_and(|status| status.split_whitespace().last() == Some("installed"))
        {
            continue;
        }
        let name = fields
            .get("Package")
            .ok_or("system-image baseline is missing a package name")?;
        installed += 1;
        if !name.starts_with("onekvm-extension-") || name == "onekvm-extension-host" {
            names.insert(name.clone());
        }
    }
    if installed == 0 {
        return Err("system-image package baseline is empty; update the sqfs system image".into());
    }
    // Also cover older images whose baseline predates one of these components.
    names.extend(
        [
            "onekvm-core",
            "onekvm-cli",
            "onekvm-ui",
            "onekvm-extension-host",
            "onekvm-plugin-manager",
        ]
        .map(str::to_owned),
    );
    Ok(names)
}

fn image_packages() -> Result<BTreeSet<String>, String> {
    let status = fs::read_to_string(BASELINE)
        .map_err(|error| format!("read system-image package baseline: {error}; {UPDATE_HINT}"))?;
    protected_packages(&status)
}

pub fn validate_local_packages<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let protected = image_packages()?;
    for name in names {
        if protected.contains(name) {
            return Err(format!(
                "local package {name} belongs to the system image; {UPDATE_HINT}"
            ));
        }
    }
    Ok(())
}

fn checked_payloads(
    files: Vec<std::path::PathBuf>,
    protected: &BTreeSet<String>,
) -> Result<std::collections::BTreeMap<String, (String, std::path::PathBuf)>, String> {
    let mut packages = std::collections::BTreeMap::new();
    for path in files {
        let fields = crate::ipk::read_package_control(&path)?;
        let name = &fields["Package"];
        if protected.contains(name) {
            return Err(format!(
                "dependency {name} belongs to the system image; {UPDATE_HINT}"
            ));
        }
        let version = &fields["Version"];
        if let Some((previous, _)) = packages.get(name) {
            if previous != version {
                return Err(format!("conflicting downloaded versions of {name}"));
            }
        } else {
            packages.insert(name.clone(), (version.clone(), path));
        }
    }
    if packages.is_empty() {
        return Err("no downloaded extension packages".into());
    }
    Ok(packages)
}

/// Freeze the downloaded transaction: validate every payload, then install with
/// no software sources configured. Keep original signed indexes untouched;
/// libsolv's --add-exclude alone is overridable by a hard dependency.
pub fn offline_install_arguments(
    tmp: &std::path::Path,
    cache: &std::path::Path,
    local: &[std::path::PathBuf],
    reinstall: bool,
) -> Result<Vec<String>, String> {
    let mut files = local.to_vec();
    for entry in fs::read_dir(cache).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_some_and(|value| value == "ipk") {
            files.push(path);
        }
    }
    let packages = checked_payloads(files, &image_packages()?)?;
    let empty = tmp.join("empty-lists");
    fs::create_dir_all(&empty).map_err(|error| error.to_string())?;
    let config = tmp.join("offline.conf");
    let mut conf = format!("dest root /\noption info_dir /var/lib/opkg/info\noption status_file /var/lib/opkg/status\noption lists_dir {}\noption cache_dir {}\n", empty.display(), cache.display());
    for line in String::from_utf8_lossy(&crate::sys::opkg_output(&["print-architecture"])?).lines()
    {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() == 3 && fields[0] == "arch" {
            conf.push_str(&format!("arch {} {}\n", fields[1], fields[2]));
        }
    }
    crate::sys::atomic_write(&config, conf.as_bytes(), 0o600)?;
    let mut arguments = vec!["--conf".into(), config.to_string_lossy().into_owned()];
    if reinstall {
        arguments.push("--force-reinstall".into());
    }
    arguments.push("install".into());
    arguments.extend(
        packages
            .into_values()
            .map(|(_, path)| path.to_string_lossy().into_owned()),
    );
    crate::install::log_install_message(
        "Verified optional dependencies; system-image packages are protected",
    );
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_libraries_and_host_are_protected_but_extensions_are_updateable() {
        let fixture = [
            "musl",
            "onekvm-extension-host",
            "onekvm-extension-ocr",
            "removed",
        ]
        .into_iter()
        .map(|name| {
            format!(
                "Package: {name}\nVersion: 1\nArchitecture: all\nStatus: {}\n\n",
                if name == "removed" {
                    "deinstall ok config-files"
                } else {
                    "install ok installed"
                }
            )
        })
        .collect::<String>();
        let names = protected_packages(&fixture).unwrap();
        assert!(names.contains("musl"));
        assert!(names.contains("onekvm-extension-host"));
        assert!(names.contains("onekvm-core"));
        assert!(!names.contains("onekvm-extension-ocr"));
        assert!(!names.contains("removed"));
        assert!(!names.contains("tailscale"));
    }

    #[test]
    fn missing_or_invalid_baseline_fails_closed() {
        assert!(protected_packages("").is_err());
        assert!(protected_packages("not a control file").is_err());
        assert!(protected_packages("Status: install ok installed").is_err());
    }

    fn fixture(
        root: &std::path::Path,
        file: &str,
        name: &str,
        version: &str,
    ) -> std::path::PathBuf {
        use std::io::Write;
        let control = format!("Package: {name}\nVersion: {version}\nArchitecture: all\n");
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, content) in [
            ("./control", control.as_bytes()),
            ("./postinst", b"#!/bin/sh\n".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            // OE emits ./control. Preserve it verbatim instead of letting the
            // tar builder normalize the path, to cover real package metadata.
            header.as_mut_bytes()[..100].fill(0);
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            archive.append(&header, content).unwrap();
        }
        let data = archive.into_inner().unwrap().finish().unwrap();
        let path = root.join(file);
        let mut output = fs::File::create(&path).unwrap();
        output.write_all(b"!<arch>\n").unwrap();
        write!(
            output,
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            "control.tar.gz/",
            0,
            0,
            0,
            "100644",
            data.len()
        )
        .unwrap();
        output.write_all(&data).unwrap();
        if data.len() % 2 != 0 {
            output.write_all(b"\n").unwrap();
        }
        path
    }

    #[test]
    fn payload_metadata_blocks_disguised_system_packages_and_allows_optional_runtimes() {
        let root = std::env::temp_dir().join(format!("onekvm-policy-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let protected = BTreeSet::from(["onekvm-core".into()]);
        let runtime = fixture(&root, "runtime.ipk", "tailscale", "1");
        assert!(checked_payloads(vec![runtime.clone()], &protected).is_ok());
        let disguised = fixture(&root, "looks-like-a-plugin.ipk", "onekvm-core", "999");
        assert!(
            checked_payloads(vec![runtime.clone(), disguised], &protected)
                .unwrap_err()
                .contains(UPDATE_HINT)
        );
        let other = fixture(&root, "other.ipk", "tailscale", "2");
        assert!(checked_payloads(vec![runtime, other], &protected)
            .unwrap_err()
            .contains("conflicting"));
        let invalid = root.join("invalid.ipk");
        fs::write(&invalid, b"not an ipk").unwrap();
        assert!(checked_payloads(vec![invalid], &protected).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
