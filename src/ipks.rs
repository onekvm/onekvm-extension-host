use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::{Archive, EntryType};

use crate::ipk::{
    clean_archive_path, inspect_control, inspect_ipk, inspect_kernel_control,
    parse_package_dependencies, read_ar, validate_package_dependencies, PackageInfo,
};
use crate::paths;

const IPKS_FORMAT: &str = "onekvm-ipks";
const MAX_PACKAGES: usize = 64;
const MAX_MANIFEST: u64 = 64 << 10;
const MAX_SIGNATURE: u64 = 8 << 10;
const MAX_TAR_METADATA: u64 = 8 << 10;
const MAX_EXPANDED: u64 = 128 << 20;
const MAX_PACKAGE: u64 = 32 << 20;
const KERNEL_BASE: &str = "onekvm-kernel-modules-base";
const IMAGE_KEYRING: &str = "/etc/opkg/gpg/pubring.kbx";
const DEVELOPMENT_MARKER: &str = "/run/onekvm/allow-unsigned-extension-packages";
const MANIFEST_NAME: &str = "bundle.json";
const SIGNATURE_NAME: &str = "bundle.json.asc";

pub fn require_unsigned_development() -> Result<(), String> {
    if !crate::feed_keys::root_owned_source(Path::new(DEVELOPMENT_MARKER)) {
        return Err("unsigned extension package; obtain a signed IPKS or authorize local development on the device".into());
    }
    Ok(())
}

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
    kernel_packages: Vec<(PathBuf, String)>,
}

impl InspectedIpks {
    pub fn validate_current_kernel(&self) -> Result<(), String> {
        for (path, payload_release) in &self.kernel_packages {
            let fields = crate::ipk::read_package_control(path)?;
            crate::kernel_identity::check_current_package(&fields)?;
            if fields.get("X-OneKVM-Kernel-Release") != Some(payload_release) {
                return Err(
                    "IPKS kernel module payload does not match its declared kernel release".into(),
                );
            }
        }
        Ok(())
    }
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
    let (manifest, extracted) = extract_ipks(filename, extraction_root)?;
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
        if item.sha256.len() != 64
            || !item
                .sha256
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(format!("IPKS package {} has invalid SHA-256", item.file));
        }
        if declared
            .insert(item.file.clone(), item.sha256.clone())
            .is_some()
        {
            return Err(format!("IPKS repeats package {}", item.file));
        }
    }
    // Authenticate every package before passing any IPK bytes to its parser.
    for (filename, package_path) in &extracted {
        let expected = declared
            .get(filename)
            .ok_or_else(|| format!("IPKS contains undeclared package {filename}"))?;
        let actual = file_sha256(package_path)?;
        if &actual != expected {
            return Err(format!(
                "IPKS package {filename} failed SHA-256 verification"
            ));
        }
    }
    let mut packages = BTreeMap::new();
    for package_path in extracted.values() {
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
    let mut kernel_packages = Vec::new();
    for (name, value) in &packages {
        if name == &manifest.root {
            continue;
        }
        validate_package_dependencies(&value.info.dependencies, Some(&bundled_names))
            .map_err(|err| format!("bundled dependency {name}: {err}"))?;
        let members = read_ar(value.path.to_str().ok_or("invalid bundled package path")?)?;
        let data = members
            .get("data.tar.gz")
            .ok_or("IPK has no gzip payload")?;
        if name.starts_with(&private_prefix) && paths::valid_package_name(name) {
            inspect_dependency_data(data, &extension_root)
                .map_err(|err| format!("bundled dependency {name}: {err}"))?;
        } else if is_kernel_support_package(name) {
            let release = inspect_kernel_dependency_data(name, data)
                .map_err(|err| format!("bundled dependency {name}: {err}"))?;
            if !kernel_release.is_empty() && kernel_release != release {
                return Err(format!(
                    "bundled kernel packages mix releases {kernel_release} and {release}"
                ));
            }
            kernel_packages.push((value.path.clone(), release.clone()));
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
        kernel_packages,
    })
}

struct BundledIpk {
    path: PathBuf,
    info: PackageInfo,
}

fn inspect_bundled(path: &Path, accepted: &BTreeSet<String>) -> Result<BundledIpk, String> {
    let members = read_ar(path.to_str().ok_or("invalid bundled package path")?)?;
    if members.get("debian-binary").map(Vec::as_slice) != Some(b"2.0\n") {
        return Err("unsupported IPK format".into());
    }
    let control = members
        .get("control.tar.gz")
        .ok_or("IPK has no control.tar.gz")?;
    let fields = inspect_control(control).or_else(|error| {
        let fields = inspect_kernel_control(control)?;
        if !is_kernel_support_package(&fields["Package"]) {
            return Err(error);
        }
        Ok(fields)
    })?;
    if !members.contains_key("data.tar.gz") {
        return Err("IPK has no gzip payload".into());
    }
    let dependencies =
        parse_package_dependencies(fields.get("Depends").map(String::as_str).unwrap_or(""))?;
    if !accepted.contains(&fields["Architecture"]) {
        return Err(format!(
            "package architecture {:?} is not accepted by opkg",
            fields["Architecture"]
        ));
    }
    Ok(BundledIpk {
        path: path.to_path_buf(),
        info: PackageInfo {
            name: fields["Package"].clone(),
            version: fields["Version"].clone(),
            architecture: fields["Architecture"].clone(),
            dependencies,
            manifest: crate::manifest::Manifest::default(),
            payload_bytes: 0,
            preview_icon_data_url: String::new(),
        },
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
        let name = clean_archive_path(&entry.path().map_err(|e| e.to_string())?.to_string_lossy())?;
        if name != root
            && !name.starts_with(&(root.to_owned() + "/"))
            && !root.starts_with(&(name.clone() + "/"))
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

pub(crate) fn inspect_kernel_dependency_data(
    package_name: &str,
    compressed: &[u8],
) -> Result<String, String> {
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
        "onekvm-kernel.json",
        "onekvm-kernel-magic",
    ];
    for (count, entry) in archive.entries().map_err(|e| e.to_string())?.enumerate() {
        if count > 10000 {
            return Err("kernel dependency has too many archive entries".into());
        }
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = clean_archive_path(&entry.path().map_err(|e| e.to_string())?.to_string_lossy())?;
        if !present.insert(name.clone()) {
            return Err(format!("duplicate archive path /{name}"));
        }
        let kind = entry.header().entry_type();
        if !matches!(kind, EntryType::Directory | EntryType::Regular) {
            return Err(format!("unsupported archive entry type for /{name}"));
        }
        let parts: Vec<&str> = name.split('/').collect();
        let is_module_tree =
            parts.len() >= 4 && parts[0] == "usr" && parts[1] == "lib" && parts[2] == "modules";
        let is_module_ancestor = name == "usr" || name == "usr/lib" || name == "usr/lib/modules";
        let is_config_ancestor = name == "etc" || name == "etc/modules-load.d";
        if matches!(kind, EntryType::Directory) {
            if is_module_ancestor || is_config_ancestor || is_module_tree {
                continue;
            }
            return Err(format!(
                "kernel dependency directory /{name} is not allowed"
            ));
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
                    return Err(format!(
                        "kernel metadata package contains unexpected file /{name}"
                    ));
                }
                modules_dep |= relative == "modules.dep";
                build_id |= relative == "onekvm-build-id";
                continue;
            }
            if relative.is_empty() || !relative.ends_with(".ko") {
                return Err(format!(
                    "kernel module package contains unexpected file /{name}"
                ));
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
            entry
                .read_to_string(&mut content)
                .map_err(|e| e.to_string())?;
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

fn validate_ipks_closure(
    root: &PackageInfo,
    packages: &BTreeMap<String, BundledIpk>,
) -> Result<(), String> {
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

fn extract_ipks(
    filename: &str,
    extraction_root: &Path,
) -> Result<(IpksManifest, BTreeMap<String, PathBuf>), String> {
    let file = fs::File::open(filename).map_err(|e| e.to_string())?;
    let decoder = flate2::bufread::GzDecoder::new(BufReader::new(file));
    let mut archive = Archive::new(decoder);
    let mut manifest = None;
    let mut manifest_data = Vec::new();
    let mut verified = false;
    let mut packages = BTreeMap::new();
    let mut expanded = 0u64;
    let mut pax_path = None;
    let mut pax_pending = false;
    for (count, entry) in archive
        .entries()
        .map_err(|e| format!("open IPKS archive: {e}"))?
        .raw(true)
        .enumerate()
    {
        if count >= 2 * MAX_PACKAGES + 4 {
            return Err("IPKS has too many archive entries".into());
        }
        let mut entry = entry.map_err(|e| e.to_string())?;
        // tar's default PAX handling collects the whole metadata record in RAM.
        // Allow bounded per-file metadata for historical and long filenames.
        if entry.header().entry_type() == EntryType::XHeader {
            if pax_pending || entry.size() > MAX_TAR_METADATA {
                return Err("invalid IPKS tar metadata".into());
            }
            if let Some(fields) = entry.pax_extensions().map_err(|e| e.to_string())? {
                for field in fields {
                    let field = field.map_err(|e| e.to_string())?;
                    match field.key_bytes() {
                        b"path" if pax_path.is_none() => {
                            pax_path = Some(field.value().map_err(|e| e.to_string())?.to_owned());
                        }
                        b"mtime" | b"atime" | b"ctime" | b"uid" | b"gid" | b"uname" | b"gname" => {}
                        _ => return Err("unsupported IPKS tar metadata".into()),
                    }
                }
            }
            pax_pending = true;
            continue;
        }
        let name = match pax_path.take() {
            Some(path) => clean_archive_path(&path)?,
            None => {
                clean_archive_path(&entry.path().map_err(|e| e.to_string())?.to_string_lossy())?
            }
        };
        pax_pending = false;
        match entry.header().entry_type() {
            EntryType::Directory => {
                if name != "packages" || entry.size() != 0 {
                    return Err(format!("IPKS has unexpected directory {name}"));
                }
            }
            EntryType::Regular => {
                if name == MANIFEST_NAME {
                    if manifest.is_some() || entry.size() > MAX_MANIFEST {
                        return Err("IPKS manifest is duplicate or too large".into());
                    }
                    entry
                        .read_to_end(&mut manifest_data)
                        .map_err(|e| e.to_string())?;
                    let value: IpksManifest = serde_json::from_slice(&manifest_data)
                        .map_err(|e| format!("decode IPKS manifest: {e}"))?;
                    match (value.format.as_str(), value.version) {
                        (IPKS_FORMAT, 1) => require_unsigned_development()?,
                        (IPKS_FORMAT, 2) => {}
                        _ => return Err("unsupported IPKS format or version".into()),
                    }
                    manifest = Some(value);
                    continue;
                }
                if name == SIGNATURE_NAME {
                    if verified
                        || manifest.as_ref().map(|value| value.version) != Some(2)
                        || entry.size() > MAX_SIGNATURE
                    {
                        return Err("unexpected, duplicate or oversized IPKS signature".into());
                    }
                    let mut signature = Vec::new();
                    entry
                        .read_to_end(&mut signature)
                        .map_err(|e| e.to_string())?;
                    verify_signature(&manifest_data, &signature, extraction_root)?;
                    verified = true;
                    continue;
                }
                match manifest.as_ref().map(|value| value.version) {
                    Some(1) => {}
                    Some(2) if verified => {}
                    Some(2) => {
                        return Err("IPKS publisher signature is missing before packages".into())
                    }
                    _ => return Err("IPKS bundle.json must precede packages".into()),
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
                    return Err(format!(
                        "IPKS package {file_name} is duplicate or too large"
                    ));
                }
                expanded += entry.size();
                if expanded > MAX_EXPANDED {
                    return Err("IPKS expanded payload is too large".into());
                }
                let destination = extraction_root.join(file_name);
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|e| e.to_string())?;
                let expected_size = entry.size();
                if std::io::copy(&mut entry, &mut output).map_err(|e| e.to_string())?
                    != expected_size
                {
                    return Err(format!("truncated IPKS package {file_name}"));
                }
                packages.insert(file_name.to_owned(), destination);
            }
            _ => return Err(format!("IPKS entry {name} is not a regular file")),
        }
    }
    if pax_pending {
        return Err("IPKS tar metadata has no file".into());
    }
    // Read the gzip footer and allow only conventional zero tar padding.
    let mut decoder = archive.into_inner();
    let mut padding = [0; 4096];
    let mut total = 0;
    loop {
        let read = decoder
            .read(&mut padding)
            .map_err(|e| format!("finish IPKS archive: {e}"))?;
        if read == 0 {
            break;
        }
        total += read;
        if total > MAX_MANIFEST as usize || padding[..read].iter().any(|&byte| byte != 0) {
            return Err("unexpected data after the IPKS archive".into());
        }
    }
    if !decoder
        .into_inner()
        .fill_buf()
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        return Err("unexpected data after the IPKS gzip stream".into());
    }
    let manifest = manifest.ok_or("IPKS bundle.json is missing")?;
    if manifest.version == 2 && !verified {
        return Err("IPKS publisher signature is missing".into());
    }
    Ok((manifest, packages))
}

fn verify_signature(manifest: &[u8], signature: &[u8], staging: &Path) -> Result<(), String> {
    if !signature.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
        return Err("invalid IPKS publisher signature format".into());
    }
    let keyring = Path::new(IMAGE_KEYRING);
    if !crate::feed_keys::root_owned_source(keyring) {
        return Err("trusted IPKS publisher keyring is unavailable".into());
    }
    let manifest_path = staging.join(MANIFEST_NAME);
    let signature_path = staging.join(SIGNATURE_NAME);
    fs::write(&manifest_path, manifest).map_err(|e| format!("stage IPKS manifest: {e}"))?;
    fs::write(&signature_path, signature).map_err(|e| format!("stage IPKS signature: {e}"))?;
    let output = Command::new("/usr/bin/gpg")
        .args([
            "--no-options",
            "--batch",
            "--no-tty",
            "--no-autostart",
            "--no-default-keyring",
            "--no-auto-key-retrieve",
            "--lock-never",
            "--trust-model",
            "always",
            "--status-fd",
            "1",
            "--homedir",
        ])
        .arg(staging)
        .arg("--keyring")
        .arg(keyring)
        .arg("--verify")
        .arg(signature_path)
        .arg(manifest_path)
        .output()
        .map_err(|e| format!("verify IPKS publisher signature: {e}"))?;
    let status = String::from_utf8_lossy(&output.stdout);
    let records: Vec<_> = status
        .lines()
        .filter_map(|line| line.strip_prefix("[GNUPG:] VALIDSIG "))
        .collect();
    // ECDSA, SHA-256, binary document (no newline canonicalization).
    let valid = records.len() == 1
        && records[0]
            .split_ascii_whitespace()
            .skip(6)
            .take(3)
            .eq(["19", "8", "00"]);
    if !output.status.success()
        || !valid
        || status.lines().any(|line| {
            matches!(
                line.split_ascii_whitespace().nth(1),
                Some("BADSIG" | "ERRSIG" | "NO_PUBKEY" | "EXPSIG" | "EXPKEYSIG" | "REVKEYSIG")
            )
        })
    {
        return Err("IPKS publisher signature verification failed".into());
    }
    Ok(())
}

fn valid_ipks_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".ipk") else {
        return false;
    };
    let mut chars = stem.chars();
    matches!(chars.next(), Some('a'..='z' | '0'..='9'))
        && stem.len() <= 127
        && chars.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '_' | '-')
        })
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut input = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    std::io::copy(&mut input, &mut digest).map_err(|e| e.to_string())?;
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
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
