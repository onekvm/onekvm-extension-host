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
#[allow(dead_code)]
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
        let info = inspect_bundled(package_path, accepted)?;
        if packages.contains_key(&info.name) {
            return Err(format!("IPKS repeats package name {}", info.name));
        }
        packages.insert(info.name.clone(), (package_path.clone(), info));
    }
    let root_path = packages
        .get(&manifest.root)
        .map(|(path, _)| path.clone())
        .ok_or_else(|| format!("IPKS root package {} is missing", manifest.root))?;
    let bundled_names: BTreeSet<String> = packages
        .keys()
        .filter(|name| *name != &manifest.root)
        .cloned()
        .collect();
    let root = inspect_ipk(
        root_path.to_str().unwrap_or_default(),
        &manifest.root,
        accepted,
        Some(&bundled_names),
    )?;
    let mut infos = BTreeMap::new();
    let mut paths = Vec::new();
    let mut names: Vec<String> = packages.keys().cloned().collect();
    names.sort();
    names.retain(|name| name != &manifest.root);
    names.push(manifest.root.clone());
    for name in names {
        if name == manifest.root {
            infos.insert(name.clone(), root.clone());
            paths.push(root_path.clone());
            continue;
        }
        let (path, info) = packages.get(&name).unwrap();
        validate_package_dependencies(&info.dependencies, Some(&bundled_names))
            .map_err(|err| format!("bundled dependency {name}: {err}"))?;
        infos.insert(name.clone(), info.clone());
        paths.push(path.clone());
    }
    Ok(InspectedIpks {
        root,
        paths,
        packages: infos,
    })
}

fn inspect_bundled(path: &Path, accepted: &BTreeSet<String>) -> Result<PackageInfo, String> {
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
    Ok(PackageInfo {
        name: fields["Package"].clone(),
        version: fields["Version"].clone(),
        architecture: fields["Architecture"].clone(),
        dependencies,
        manifest: crate::manifest::Manifest::default(),
        preview_icon_data_url: String::new(),
    })
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

#[allow(dead_code)]
pub fn is_kernel_support_package(name: &str) -> bool {
    name == KERNEL_BASE || name.starts_with("onekvm-kernel-module-")
}
