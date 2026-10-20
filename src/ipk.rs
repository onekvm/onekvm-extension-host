use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::Path;

use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

use crate::control::parse_control;
use crate::manifest::{decode_manifest, Manifest};

const MAX_IPK_MEMBER: u64 = 256 << 20;
const MAX_PREVIEW_ICON: u64 = 1 << 20;
const MAX_LAYOUT: u64 = 64 * 1024;

// Platform libs plus companion program packages split out of extension recipes
// (feed install downloads the extension IPK only; opkg still resolves these).
// `onekvm-kernel-module-*` is allowed the same way: opkg installs the signed
// module package. An IPKS bundle still inspects any module payload it carries.
const PLATFORM_DEPENDENCIES: &[&str] = &[
    "cloudflared",
    "ddns-go",
    "easytier",
    "frpc",
    "kmod",
    "libcrypto3",
    "libgcc1",
    "libjson-c5",
    "libsqlite3-0",
    "libssl3",
    "libstdc++6",
    "libsystemd0",
    "musl",
    "netbird",
    "newt",
    "npc",
    "onekvm-core",
    "onekvm-ui",
    "onekvm-extension-host",
    "onekvm-plugin-manager",
    "onekvm-samba-runtime",
    "rathole",
    "tailscale",
    "vnt",
    "wireguard-tools",
    "zerotier-one",
];

#[derive(Debug, Clone)]
pub struct PackageDependency {
    pub name: String,
    #[allow(dead_code)]
    pub compare: String,
    #[allow(dead_code)]
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub dependencies: Vec<PackageDependency>,
    pub manifest: Manifest,
    pub preview_icon_data_url: String,
}

pub fn inspect_ipk(
    filename: &str,
    expected_package: &str,
    accepted: &BTreeSet<String>,
    bundled: Option<&BTreeSet<String>>,
) -> Result<PackageInfo, String> {
    let members = read_ar(filename)?;
    if members.get("debian-binary").map(Vec::as_slice) != Some(b"2.0\n") {
        return Err("unsupported IPK format".into());
    }
    let control = members
        .get("control.tar.gz")
        .ok_or_else(|| "IPK has no control.tar.gz".to_string())?;
    let fields = inspect_control(control)?;
    let dependencies =
        parse_package_dependencies(fields.get("Depends").map(String::as_str).unwrap_or(""))?;
    let info_name = fields["Package"].clone();
    if !expected_package.is_empty() && info_name != expected_package {
        return Err(format!(
            "downloaded package is {info_name:?}, expected {expected_package:?}"
        ));
    }
    if !crate::paths::valid_package_name(&info_name) {
        return Err(format!(
            "package {info_name:?} is outside the extension namespace"
        ));
    }
    if !accepted.contains(&fields["Architecture"]) {
        return Err(format!(
            "package architecture {:?} is not accepted by opkg",
            fields["Architecture"]
        ));
    }
    validate_package_dependencies(&dependencies, bundled)?;
    let data = match members.get("data.tar.gz") {
        Some(data) => data,
        None if members.contains_key("data.tar.zst") => {
            return Err("extension IPK uses zstd; rebuild it with onekvm-extension.bbclass".into());
        }
        None => return Err("IPK has no supported data archive".into()),
    };
    let expected_id = info_name.trim_start_matches("onekvm-extension-");
    let manifest = inspect_data(data, expected_id)?;
    let preview = extract_preview_icon(data, &manifest)?;
    Ok(PackageInfo {
        name: info_name,
        version: fields["Version"].clone(),
        architecture: fields["Architecture"].clone(),
        dependencies,
        manifest,
        preview_icon_data_url: preview,
    })
}

pub fn inspect_control(compressed: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let decoder = GzDecoder::new(compressed);
    let mut archive = Archive::new(decoder);
    let mut control = None;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .path()
            .map_err(|e| e.to_string())?
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        match name.as_str() {
            "preinst" | "postinst" | "prerm" | "postrm" => {
                return Err(format!("package maintenance script {name} is forbidden"));
            }
            "control" => {
                if entry.size() > 64 * 1024 {
                    return Err("control file is too large".into());
                }
                let mut data = Vec::new();
                entry
                    .read_to_end(&mut data)
                    .map_err(|e| format!("read control: {e}"))?;
                control = Some(data);
            }
            _ => {}
        }
    }
    parse_control(&control.ok_or_else(|| "package control file is missing".to_string())?)
}

pub fn parse_package_dependencies(value: &str) -> Result<Vec<PackageDependency>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = BTreeSet::new();
    let mut dependencies = Vec::new();
    for field in value.split(',') {
        let dependency = field.trim();
        let (name, compare, version) = parse_dependency(dependency)?;
        if !seen.insert(name.clone()) {
            return Err(format!("extension package repeats dependency {name:?}"));
        }
        dependencies.push(PackageDependency {
            name,
            compare,
            version,
        });
    }
    Ok(dependencies)
}

fn parse_dependency(value: &str) -> Result<(String, String, String), String> {
    if let Some((name, rest)) = value.split_once(" (") {
        let rest = rest
            .strip_suffix(')')
            .ok_or_else(|| format!("extension package has invalid dependency {value:?}"))?;
        let mut parts = rest.splitn(2, ' ');
        let compare = parts.next().unwrap_or_default();
        let version = parts.next().unwrap_or_default();
        if !matches!(compare, "=" | ">=" | "<=" | ">" | "<") || version.is_empty() {
            return Err(format!(
                "extension package has invalid dependency {value:?}"
            ));
        }
        validate_dep_name(name)?;
        return Ok((name.to_owned(), compare.to_owned(), version.to_owned()));
    }
    validate_dep_name(value)?;
    Ok((value.to_owned(), String::new(), String::new()))
}

fn validate_dep_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    if matches!(chars.next(), Some('a'..='z' | '0'..='9'))
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
    {
        return Ok(());
    }
    Err(format!("extension package has invalid dependency {name:?}"))
}

fn feed_dependency_allowed(name: &str) -> bool {
    PLATFORM_DEPENDENCIES.contains(&name) || name.starts_with("onekvm-kernel-module-")
}

pub fn validate_package_dependencies(
    dependencies: &[PackageDependency],
    bundled: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    for dependency in dependencies {
        let allowed = feed_dependency_allowed(&dependency.name)
            || bundled.is_some_and(|set| set.contains(&dependency.name));
        if !allowed {
            return Err(format!(
                "extension package has unsupported dependency {:?}",
                dependency.name
            ));
        }
    }
    Ok(())
}

pub fn inspect_data(compressed: &[u8], expected_id: &str) -> Result<Manifest, String> {
    let decoder = GzDecoder::new(compressed);
    let mut archive = Archive::new(decoder);
    let mut entries = Vec::new();
    let mut manifest_data = None;
    let mut regular_json = BTreeMap::new();
    for (count, entry) in archive.entries().map_err(|e| e.to_string())?.enumerate() {
        if count > 10000 {
            return Err("extension has too many archive entries".into());
        }
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = clean_archive_path(&entry.path().map_err(|e| e.to_string())?.to_string_lossy())?;
        let kind = entry.header().entry_type();
        if !matches!(
            kind,
            EntryType::Directory | EntryType::Regular | EntryType::Symlink | EntryType::Link
        ) {
            return Err(format!("unsupported archive entry type for {name}"));
        }
        let linkname = entry
            .link_name()
            .ok()
            .flatten()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mode = entry.header().mode().unwrap_or(0);
        entries.push((name.clone(), linkname, mode, kind));
        if Path::new(&name)
            .file_name()
            .and_then(|value| value.to_str())
            == Some("manifest.json")
            && matches!(kind, EntryType::Regular)
        {
            if entry.size() > 64 * 1024 {
                return Err("manifest is too large".into());
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
            manifest_data = Some(data);
        } else if name.contains("/web/")
            && name.ends_with(".json")
            && matches!(kind, EntryType::Regular)
        {
            if entry.size() > MAX_LAYOUT {
                regular_json.insert(name, None);
                continue;
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
            regular_json.insert(name, Some(data));
        }
    }
    let manifest =
        decode_manifest(&manifest_data.ok_or_else(|| "manifest.json is missing".to_string())?)?;
    if manifest.id != expected_id {
        return Err(format!(
            "manifest id {:?} does not match package",
            manifest.id
        ));
    }
    let root = format!("data/onekvm/apps/{}/{}", manifest.id, manifest.version);
    let mut present = BTreeMap::new();
    for (name, linkname, mode, kind) in &entries {
        if name != &root
            && !name.starts_with(&(root.clone() + "/"))
            && !root.starts_with(&(name.clone() + "/"))
        {
            return Err(format!("payload path /{name} escapes /{root}"));
        }
        if matches!(kind, EntryType::Symlink | EntryType::Link) {
            let target = if matches!(kind, EntryType::Symlink) {
                Path::new(name)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(linkname)
                    .to_string_lossy()
                    .into_owned()
            } else {
                linkname.clone()
            };
            let clean = clean_archive_path(&target)?;
            if clean != root && !clean.starts_with(&(root.clone() + "/")) {
                return Err(format!("link /{name} escapes extension root"));
            }
        }
        if present.contains_key(name) {
            return Err(format!("duplicate archive path /{name}"));
        }
        present.insert(name.clone(), (*mode, *kind));
    }
    let mut executables = Vec::new();
    if !manifest.entrypoint.is_empty() {
        executables.push(manifest.entrypoint.clone());
    }
    if let Some(controller) = &manifest.controller {
        executables.push(controller.entrypoint.clone());
    }
    for method in manifest.methods.values() {
        if let Some(command) = method.command.first() {
            executables.push(command.clone());
        }
    }
    if let Some(command) = manifest.healthcheck.command.first() {
        executables.push(command.clone());
    }
    if let Some(command) = manifest.lifecycle.enable.first() {
        executables.push(command.clone());
    }
    if let Some(command) = manifest.lifecycle.disable.first() {
        executables.push(command.clone());
    }
    for executable in executables {
        let path = format!("{root}/{executable}");
        let Some((mode, kind)) = present.get(&path) else {
            return Err(format!("{executable} is not an executable regular file"));
        };
        if !matches!(kind, EntryType::Regular) || mode & 0o111 == 0 {
            return Err(format!("{executable} is not an executable regular file"));
        }
    }
    if let Some(page) = &manifest.page {
        let path = format!("{root}/{}", page.entrypoint);
        let Some((_, kind)) = present.get(&path) else {
            return Err(format!("{} is not a regular file", page.entrypoint));
        };
        if !matches!(kind, EntryType::Regular) {
            return Err(format!("{} is not a regular file", page.entrypoint));
        }
        if page.renderer == "layout" {
            match regular_json.get(&path) {
                Some(Some(_)) => {}
                _ => return Err(format!("{} is missing or too large", page.entrypoint)),
            }
        }
        if let Some(icon) = &page.icon {
            if icon.source == "custom" {
                require_regular(&present, &root, &icon.path)?;
            }
        }
    }
    if let Some(toolbar) = &manifest.toolbar {
        require_regular(&present, &root, &toolbar.entrypoint)?;
    }
    if let Some(shell) = &manifest.shell {
        require_regular(&present, &root, &shell.entrypoint)?;
    }
    if let Some(toolbox) = &manifest.toolbox {
        for item in &toolbox.items {
            require_regular(&present, &root, &item.view.entrypoint)?;
            if let Some(crate::manifest::ToolboxIcon::Custom { path }) = &item.icon {
                require_regular(&present, &root, path)?;
            }
        }
    }
    if let Some(icon) = &manifest.icon {
        if icon.source == "custom" {
            require_regular(&present, &root, &icon.path)?;
        }
    }
    Ok(manifest)
}

fn require_regular(
    present: &BTreeMap<String, (u32, EntryType)>,
    root: &str,
    relative: &str,
) -> Result<(), String> {
    let path = format!("{root}/{relative}");
    match present.get(&path) {
        Some((_, kind)) if matches!(kind, EntryType::Regular) => Ok(()),
        _ => Err(format!("{relative} is not a regular file")),
    }
}

fn extract_preview_icon(compressed: &[u8], manifest: &Manifest) -> Result<String, String> {
    let icon = manifest
        .icon
        .as_ref()
        .or(manifest.page.as_ref().and_then(|page| page.icon.as_ref()));
    let Some(icon) = icon else {
        return Ok(String::new());
    };
    if icon.source != "custom" {
        return Ok(String::new());
    }
    let mime = match Path::new(&icon.path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => return Ok(String::new()),
    };
    let target = format!(
        "data/onekvm/apps/{}/{}/{}",
        manifest.id, manifest.version, icon.path
    );
    let decoder = GzDecoder::new(compressed);
    let mut archive = Archive::new(decoder);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = clean_archive_path(&entry.path().map_err(|e| e.to_string())?.to_string_lossy())?;
        if name != target {
            continue;
        }
        if entry.size() > MAX_PREVIEW_ICON {
            return Ok(String::new());
        }
        let mut content = Vec::new();
        entry.read_to_end(&mut content).map_err(|e| e.to_string())?;
        return Ok(format!("data:{mime};base64,{}", base64_encode(&content)));
    }
    Ok(String::new())
}

pub fn clean_archive_path(name: &str) -> Result<String, String> {
    let name = name.trim_start_matches("./");
    if name.is_empty() || name.starts_with('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!("unsafe archive path {name:?}"));
    }
    let mut parts = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("unsafe archive path {name:?}")),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(format!("unsafe archive path {name:?}"));
    }
    Ok(parts.join("/"))
}

pub fn read_ar(filename: &str) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut file = std::fs::File::open(filename).map_err(|e| e.to_string())?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)
        .map_err(|_| "invalid ar archive".to_string())?;
    if &magic != b"!<arch>\n" {
        return Err("invalid ar archive".into());
    }
    let mut members = BTreeMap::new();
    loop {
        let mut header = [0u8; 60];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(_) => return Err("invalid ar member header".into()),
        }
        if &header[58..60] != b"`\n" {
            return Err("invalid ar member header".into());
        }
        let name = std::str::from_utf8(&header[..16])
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_owned();
        let size: i64 = std::str::from_utf8(&header[48..58])
            .unwrap_or_default()
            .trim()
            .parse()
            .map_err(|_| "invalid ar member size".to_string())?;
        if size < 0 || size as u64 > MAX_IPK_MEMBER {
            return Err("invalid ar member size".into());
        }
        let mut content = vec![0u8; size as usize];
        file.read_exact(&mut content)
            .map_err(|e| format!("read ar member {name}: {e}"))?;
        if members.contains_key(&name) {
            return Err(format!("duplicate ar member {name}"));
        }
        members.insert(name, content);
        if size % 2 != 0 {
            let mut padding = [0u8; 1];
            file.read_exact(&mut padding)
                .map_err(|_| "read ar padding".to_string())?;
        }
    }
    Ok(members)
}

#[allow(dead_code)]
pub fn members_from_bytes(data: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut cursor = Cursor::new(data);
    let mut magic = [0u8; 8];
    cursor
        .read_exact(&mut magic)
        .map_err(|_| "invalid ar archive".to_string())?;
    if &magic != b"!<arch>\n" {
        return Err("invalid ar archive".into());
    }
    let mut members = BTreeMap::new();
    loop {
        let mut header = [0u8; 60];
        match cursor.read_exact(&mut header) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(_) => return Err("invalid ar member header".into()),
        }
        if &header[58..60] != b"`\n" {
            return Err("invalid ar member header".into());
        }
        let name = std::str::from_utf8(&header[..16])
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_owned();
        let size: i64 = std::str::from_utf8(&header[48..58])
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(-1);
        if size < 0 {
            return Err("invalid ar member size".into());
        }
        let mut content = vec![0u8; size as usize];
        cursor.read_exact(&mut content).map_err(|e| e.to_string())?;
        members.insert(name, content);
        if size % 2 != 0 {
            let mut padding = [0u8; 1];
            let _ = cursor.read_exact(&mut padding);
        }
    }
    Ok(members)
}

pub(crate) fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{clean_archive_path, parse_package_dependencies, validate_package_dependencies};

    #[test]
    fn rejects_escaping_archive_paths() {
        assert!(clean_archive_path("../etc/passwd").is_err());
        assert_eq!(
            clean_archive_path("./data/onekvm/apps/edid/1/manifest.json").unwrap(),
            "data/onekvm/apps/edid/1/manifest.json"
        );
    }

    #[test]
    fn rejects_unknown_dependencies() {
        let deps = parse_package_dependencies("curl").unwrap();
        assert!(validate_package_dependencies(&deps, None).is_err());
        let deps = parse_package_dependencies("onekvm-core, musl").unwrap();
        validate_package_dependencies(&deps, None).unwrap();
    }

    #[test]
    fn accepts_sqlite_runtime_dependency() {
        let deps = parse_package_dependencies("libsqlite3-0 (>= 3.51.3)").unwrap();
        validate_package_dependencies(&deps, None).unwrap();
    }

    #[test]
    fn accepts_extension_companion_program_packages() {
        let deps = parse_package_dependencies("ddns-go, frpc, cloudflared").unwrap();
        validate_package_dependencies(&deps, None).unwrap();
    }

    #[test]
    fn accepts_wireguard_plugin_dependencies() {
        let deps =
            parse_package_dependencies("wireguard-tools, onekvm-kernel-module-wireguard").unwrap();
        validate_package_dependencies(&deps, None).unwrap();
    }
}
