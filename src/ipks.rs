use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::{Archive, EntryType};

use crate::ipk::{
    clean_archive_path, inspect_control, inspect_ipk, parse_package_dependencies, read_ar,
    validate_package_dependencies, PackageInfo,
};
use crate::paths;

const IPKS_FORMAT: &str = "onekvm-ipks";
const MAX_PACKAGES: usize = 64;
const MAX_MANIFEST: u64 = 64 << 10;
const MAX_EXPANDED: u64 = 128 << 20;
const MAX_PACKAGE: u64 = 32 << 20;
const KERNEL_BASE: &str = "onekvm-kernel-modules-base";

#[derive(Debug, Deserialize)]
struct IpksManifest {
    format: String,
    version: i32,
    root: String,
    packages: Vec<IpksManifestPackage>,
}

#[derive(Debug, Deserialize)]
struct IpksManifestPackage {
    file: String,
    sha256: String,
}

pub struct InspectedIpks {
    pub root: PackageInfo,
    pub paths: Vec<PathBuf>,
    pub packages: BTreeMap<String, PackageInfo>,
}

pub fn is_ipks(filename: &str) -> Result<bool, String> {
    let mut file = fs::File::open(filename).map_err(|e| e.to_string())?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).map_err(|e| e.to_string())?;
    Ok(magic == [0x1f, 0x8b])
}

pub fn inspect_ipks(
    filename: &str,
    extraction_root: &Path,
    accepted: &BTreeSet<String>,
) -> Result<InspectedIpks, String> {
    let (manifest_data, extracted) = extract_ipks(filename, extraction_root)?;
    let manifest: IpksManifest = serde_json::from_slice(&manifest_data)
        .map_err(|e| format!("decode IPKS manifest: {e}"))?;
    if manifest.format != IPKS_FORMAT || manifest.version != 1 {
        return Err("unsupported IPKS format or version".into());
    }
    if !paths::valid_package_name(&manifest.root) {
        return Err(format!("IPKS has invalid root package {:?}", manifest.root));
    }
    if manifest.packages.is_empty() || manifest.packages.len() > MAX_PACKAGES {
        return Err(format!("IPKS must contain 1 to {MAX_PACKAGES} packages"));
    }
    if extracted.len() != manifest.packages.len() {
        return Err("IPKS package list does not match archive contents".into());
    }
    let mut declared = BTreeMap::new();
    for item in &manifest.packages {
        if !valid_ipks_filename(&item.file) {
            return Err(format!("IPKS has invalid package filename {:?}", item.file));
        }
        if item.sha256.len() != 64 || !item.sha256.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(format!("IPKS package {} has invalid SHA-256", item.file));
        }
        if declared.insert(item.file.clone(), item.sha256.clone()).is_some() {
            return Err(format!("IPKS repeats package {}", item.file));
        }
    }
    let mut packages = BTreeMap::new();
    for (filename, package_path) in &extracted {
        let expected = declared
            .get(filename)
            .ok_or_else(|| format!("IPKS contains undeclared package {filename}"))?;
        let actual = file_sha256(package_path)?;
        if &actual != expected {
            return Err(format!("IPKS package {filename} failed SHA-256 verification"));
        }
        let bundled = inspect_bundled(package_path, accepted)?;
        if packages.contains_key(&bundled.info.name) {
            return Err(format!("IPKS repeats package name {}", bundled.info.name));
        }
        packages.insert(bundled.info.name.clone(), bundled);
    }
    let bundled_names: BTreeSet<String> = packages
        .keys()
        .filter(|name| *name != &manifest.root)
        .cloned()
        .collect();
    let root_path = packages
        .get(&manifest.root)
        .map(|value| value.path.clone())
        .ok_or_else(|| format!("IPKS root package {} is missing", manifest.root))?;
    let root = inspect_ipk(
        root_path.to_str().unwrap_or_default(),
        &manifest.root,
        accepted,
        Some(&bundled_names),
    )?;
    let extension_root = format!(
        "data/onekvm/apps/{}/{}",
        root.manifest.id, root.manifest.version
    );
    let private_prefix = format!("{}-", manifest.root);
    let mut kernel_release = String::new();
    for (name, value) in &packages {
        if name == &manifest.root {
            continue;
        }
        validate_package_dependencies(&value.info.dependencies, Some(&bundled_names))
            .map_err(|err| format!("bundled dependency {name}: {err}"))?;
        if name.starts_with(&private_prefix) && paths::valid_package_name(name) {
            inspect_dependency_data(&value.data, &extension_root)
                .map_err(|err| format!("bundled dependency {name}: {err}"))?;
        } else if is_kernel_support_package(name) {
            let release = inspect_kernel_dependency_data(name, &value.data)
                .map_err(|err| format!("bundled dependency {name}: {err}"))?;
            if !kernel_release.is_empty() && kernel_release != release {
                return Err(format!(
                    "bundled kernel packages mix releases {kernel_release} and {release}"
                ));
            }
            kernel_release = release;
        } else {
            return Err(format!(
                "bundled dependency {name} is outside the extension or kernel support namespaces"
            ));
        }
    }
    validate_ipks_closure(&root, &packages)?;
    validate_bundled_versions(&packages)?;
    let mut infos = BTreeMap::new();
    let mut paths = Vec::new();
    let mut names: Vec<String> = packages.keys().cloned().collect();
    names.sort();
    names.retain(|name| name != &manifest.root);
    names.push(manifest.root.clone());
    for name in names {
        if name == manifest.root {
            infos.insert(name, root.clone());
            paths.push(root_path.clone());
            continue;
        }
        let value = packages.get(&name).unwrap();
        infos.insert(name, value.info.clone());
        paths.push(value.path.clone());
    }
    Ok(InspectedIpks {
        root,
        paths,
        packages: infos,
    })
}

struct BundledIpk {
    path: PathBuf,
    info: PackageInfo,
    data: Vec<u8>,
}

fn inspect_bundled(path: &Path, accepted: &BTreeSet<String>) -> Result<BundledIpk, String> {
    let members = read_ar(path.to_str().unwrap_or_default())?;
    if members.get("debian-binary").map(Vec::as_slice) != Some(b"2.0\n") {
        return Err("unsupported IPK format".into());
    }
    let control = members
        .get("control.tar.gz")
        .ok_or_else(|| "IPK has no control.tar.gz".to_string())?;
    let fields = inspect_control(control)?;
    let dependencies = parse_package_dependencies(fields.get("Depends").map(String::as_str).unwrap_or(""))?;
    if !accepted.contains(&fields["Architecture"]) {
        return Err(format!(
            "package architecture {:?} is not accepted by opkg",
            fields["Architecture"]
        ));
    }
    let data = members
        .get("data.tar.gz")
        .cloned()
        .ok_or_else(|| "IPK has no supported data archive".to_string())?;
    Ok(BundledIpk {
        path: path.to_path_buf(),
        info: PackageInfo {
            name: fields["Package"].clone(),
            version: fields["Version"].clone(),
            architecture: fields["Architecture"].clone(),
            dependencies,
            manifest: crate::manifest::Manifest::default(),
            preview_icon_data_url: String::new(),
        },
        data,
    })
}

fn inspect_dependency_data(compressed: &[u8], root: &str) -> Result<(), String> {
    let decoder = GzDecoder::new(compressed);
    let mut archive = Archive::new(decoder);
    let mut present = BTreeSet::new();
    let mut files = 0;
    for (count, entry) in archive.entries().map_err(|e| e.to_string())?.enumerate() {
        if count > 10000 {
            return Err("dependency has too many archive entries".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let name = clean_archive_path(
            &entry
                .path()
                .map_err(|e| e.to_string())?
                .to_string_lossy(),
        )?;
        if name != root && !name.starts_with(&(root.to_owned() + "/")) && !root.starts_with(&(name.clone() + "/"))
        {
            return Err(format!("payload path /{name} escapes /{root}"));
        }
        if !present.insert(name.clone()) {
            return Err(format!("duplicate archive path /{name}"));
        }
        match entry.header().entry_type() {
            EntryType::Directory => {}
            EntryType::Regular => {
                files += 1;
                if name == format!("{root}/manifest.json") {
                    return Err("dependency may not provide manifest.json".into());
                }
            }
            EntryType::Symlink | EntryType::Link => {
                let linkname = entry
                    .link_name()
                    .ok()
                    .flatten()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let target = if matches!(entry.header().entry_type(), EntryType::Symlink) {
                    Path::new(&name)
                        .parent()
                        .unwrap_or_else(|| Path::new(""))
                        .join(linkname)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    linkname
                };
                let clean = clean_archive_path(&target)?;
                if clean != root && !clean.starts_with(&(root.to_owned() + "/")) {
                    return Err(format!("link /{name} escapes extension root"));
                }
            }
            _ => return Err(format!("unsupported archive entry type for {name}")),
        }
    }
    if files == 0 {
        return Err("dependency payload is empty".into());
    }
    Ok(())
}

fn inspect_kernel_dependency_data(package_name: &str, compressed: &[u8]) -> Result<String, String> {
    let decoder = GzDecoder::new(compressed);
    let mut archive = Archive::new(decoder);
    let mut present = BTreeSet::new();
    let mut releases = BTreeSet::new();
    let mut ko_files = 0;
    let mut modules_dep = false;
    let mut build_id = false;
    let expected_module = package_name.trim_start_matches("onekvm-kernel-module-");
    let metadata = [
        "modules.alias",
        "modules.alias.bin",
        "modules.builtin",
        "modules.builtin.alias.bin",
        "modules.builtin.bin",
        "modules.builtin.modinfo",
        "modules.dep",
        "modules.dep.bin",
        "modules.devname",
        "modules.order",
        "modules.softdep",
        "modules.symbols",
        "modules.symbols.bin",
        "onekvm-build-id",
    ];
    for (count, entry) in archive.entries().map_err(|e| e.to_string())?.enumerate() {
        if count > 10000 {
            return Err("kernel dependency has too many archive entries".into());
        }
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = clean_archive_path(
            &entry
                .path()
                .map_err(|e| e.to_string())?
                .to_string_lossy(),
        )?;
        if !present.insert(name.clone()) {
            return Err(format!("duplicate archive path /{name}"));
        }
        let kind = entry.header().entry_type();
        if !matches!(kind, EntryType::Directory | EntryType::Regular) {
            return Err(format!("unsupported archive entry type for /{name}"));
        }
        let parts: Vec<&str> = name.split('/').collect();
        let is_module_tree = parts.len() >= 4 && parts[0] == "usr" && parts[1] == "lib" && parts[2] == "modules";
        let is_module_ancestor = name == "usr" || name == "usr/lib" || name == "usr/lib/modules";
        let is_config_ancestor = name == "etc" || name == "etc/modules-load.d";
        if matches!(kind, EntryType::Directory) {
            if is_module_ancestor || is_config_ancestor || is_module_tree {
                continue;
            }
            return Err(format!("kernel dependency directory /{name} is not allowed"));
        }
        if is_module_tree {
            let release = parts[3];
            if !valid_kernel_release(release) {
                return Err(format!("invalid kernel release directory {release:?}"));
            }
            releases.insert(release.to_owned());
            let relative = parts[4..].join("/");
            if package_name == KERNEL_BASE {
                if relative.contains('/') || !metadata.contains(&relative.as_str()) {
                    return Err(format!("kernel metadata package contains unexpected file /{name}"));
                }
                modules_dep |= relative == "modules.dep";
                build_id |= relative == "onekvm-build-id";
                continue;
            }
            if relative.is_empty() || !relative.ends_with(".ko") {
                return Err(format!("kernel module package contains unexpected file /{name}"));
            }
            ko_files += 1;
            continue;
        }
        if package_name != KERNEL_BASE
            && name.starts_with("etc/modules-load.d/")
            && name.ends_with(".conf")
        {
            if entry.size() > 4096 {
                return Err("kernel module load configuration is too large".into());
            }
            let mut content = String::new();
            entry.read_to_string(&mut content).map_err(|e| e.to_string())?;
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if line != expected_module {
                    return Err(format!(
                        "modules-load entry {line:?} does not match package module {expected_module:?}"
                    ));
                }
            }
            continue;
        }
        return Err(format!("kernel dependency file /{name} is not allowed"));
    }
    if releases.len() != 1 {
        return Err("kernel dependency must target exactly one release".into());
    }
    let release = releases.into_iter().next().unwrap();
    if package_name == KERNEL_BASE {
        if !modules_dep || !build_id {
            return Err("kernel metadata package requires modules.dep and onekvm-build-id".into());
        }
    } else if ko_files == 0 {
        return Err("kernel module package requires at least one .ko".into());
    }
    Ok(release)
}

fn valid_kernel_release(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && value.len() <= 127
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~' | '-'))
}

fn validate_bundled_versions(packages: &BTreeMap<String, BundledIpk>) -> Result<(), String> {
    for value in packages.values() {
        for dependency in &value.info.dependencies {
            let Some(candidate) = packages.get(&dependency.name) else {
                continue;
            };
            if dependency.compare.is_empty() {
                continue;
            }
            if crate::sys::opkg(&[
                "compare-versions",
                &candidate.info.version,
                &dependency.compare,
                &dependency.version,
            ])
            .is_err()
            {
                return Err(format!(
                    "package {} requires {} ({} {}), bundle contains {}",
                    value.info.name,
                    dependency.name,
                    dependency.compare,
                    dependency.version,
                    candidate.info.version
                ));
            }
        }
    }
    Ok(())
}

fn validate_ipks_closure(root: &PackageInfo, packages: &BTreeMap<String, BundledIpk>) -> Result<(), String> {
    let mut reachable = BTreeSet::from([root.name.clone()]);
    let mut queue = vec![root.clone()];
    while let Some(current) = queue.pop() {
        for dependency in &current.dependencies {
            let Some(value) = packages.get(&dependency.name) else {
                continue;
            };
            if reachable.insert(dependency.name.clone()) {
                queue.push(value.info.clone());
            }
        }
    }
    for name in packages.keys() {
        if !reachable.contains(name) {
            return Err(format!("IPKS contains unreferenced package {name}"));
        }
    }
    Ok(())
}

fn extract_ipks(filename: &str, extraction_root: &Path) -> Result<(Vec<u8>, BTreeMap<String, PathBuf>), String> {
    let file = fs::File::open(filename).map_err(|e| e.to_string())?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let mut manifest = None;
    let mut packages = BTreeMap::new();
    let mut expanded = 0u64;
    for (count, entry) in archive.entries().map_err(|e| format!("open IPKS archive: {e}"))?.enumerate() {
        if count > MAX_PACKAGES + 4 {
            return Err("IPKS has too many archive entries".into());
        }
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = clean_archive_path(
            &entry
                .path()
                .map_err(|e| e.to_string())?
                .to_string_lossy(),
        )?;
        match entry.header().entry_type() {
            EntryType::Directory => {
                if name != "packages" {
                    return Err(format!("IPKS has unexpected directory {name}"));
                }
            }
            EntryType::Regular => {
                expanded += entry.size();
                if expanded > MAX_EXPANDED {
                    return Err("IPKS expanded payload is too large".into());
                }
                if name == "bundle.json" {
                    if manifest.is_some() || entry.size() > MAX_MANIFEST {
                        return Err("IPKS manifest is duplicate or too large".into());
                    }
                    let mut data = Vec::new();
                    entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
                    manifest = Some(data);
                    continue;
                }
                let file_name = Path::new(&name)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                if Path::new(&name).parent().and_then(|value| value.to_str()) != Some("packages")
                    || !valid_ipks_filename(file_name)
                {
                    return Err(format!("IPKS has unexpected entry {name}"));
                }
                if packages.contains_key(file_name) || entry.size() > MAX_PACKAGE {
                    return Err(format!("IPKS package {file_name} is duplicate or too large"));
                }
                let destination = extraction_root.join(file_name);
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut output).map_err(|e| e.to_string())?;
                packages.insert(file_name.to_owned(), destination);
            }
            _ => return Err(format!("IPKS entry {name} is not a regular file")),
        }
    }
    Ok((
        manifest.ok_or_else(|| "IPKS bundle.json is missing".to_string())?,
        packages,
    ))
}

fn valid_ipks_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".ipk") else {
        return false;
    };
    let mut chars = stem.chars();
    matches!(chars.next(), Some('a'..='z' | '0'..='9'))
        && stem.len() <= 127
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '_' | '-'))
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    Ok(Sha256::digest(&data).iter().map(|byte| format!("{byte:02x}")).collect())
}

fn is_kernel_support_package(name: &str) -> bool {
    name == KERNEL_BASE || name.starts_with("onekvm-kernel-module-")
}

#[cfg(test)]
mod tests {
    use super::inspect_dependency_data;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::{Builder, Header};

    fn gzip_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut archive = Builder::new(&mut encoder);
            for (name, data) in entries {
                let mut header = Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                archive.append_data(&mut header, name, *data).unwrap();
            }
            archive.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[test]
    fn rejects_escaping_dependency_payload() {
        let data = gzip_tar(&[("etc/passwd", b"root")]);
        assert!(inspect_dependency_data(&data, "data/onekvm/apps/edid/1").is_err());
    }
}
