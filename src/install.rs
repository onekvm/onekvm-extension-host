use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::catalog::{summary_from_manifest, CatalogSummary};
use crate::control::parse_control;
use crate::host;
use crate::ipk::{inspect_ipk, PackageInfo};
use crate::ipks::{inspect_ipks, is_ipks};
use crate::lifecycle;
use crate::manifest::{load_installed, installed_versions};
use crate::memory;
use crate::paths;
use crate::state::{read_state, write_state};
use crate::sys;

#[derive(Debug, Serialize)]
pub struct PackagePreview {
    #[serde(flatten)]
    pub summary: CatalogSummary,
    pub package_name: String,
    pub architecture: String,
    pub bundle: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<LocalPackageContent>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub preview_icon_data_url: String,
}

#[derive(Debug, Serialize)]
pub struct LocalPackageContent {
    pub name: String,
    pub version: String,
    pub architecture: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub root: bool,
    pub installed: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub installed_version: String,
}

struct InspectedUpload {
    info: PackageInfo,
    paths: Vec<PathBuf>,
    bundle: bool,
    packages: std::collections::BTreeMap<String, PackageInfo>,
    cleanup: Option<PathBuf>,
}

impl Drop for InspectedUpload {
    fn drop(&mut self) {
        if let Some(path) = &self.cleanup {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub fn validate_local_upload_path(filename: &str) -> Result<(), String> {
    let clean = PathBuf::from(path_clean(Path::new(filename)));
    if clean.parent() != Some(Path::new(paths::UPLOAD_ROOT))
        || !paths::valid_upload_name(
            clean
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default(),
        )
    {
        return Err("local extension path is outside the upload staging directory".into());
    }
    let info = fs::symlink_metadata(&clean).map_err(|e| format!("read staged extension package: {e}"))?;
    if !info.file_type().is_file() {
        return Err("staged extension package is not a regular file".into());
    }
    if info.len() == 0 {
        return Err("local extension package is empty".into());
    }
    if info.len() > paths::MAX_LOCAL_IPK {
        return Err("local extension package or bundle exceeds 32 MiB".into());
    }
    Ok(())
}

fn path_clean(path: &Path) -> String {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    if parts.is_empty() {
        "/".into()
    } else if path.is_absolute() {
        format!("/{}", parts.join("/").trim_start_matches('/'))
    } else {
        parts.join("/")
    }
}

pub fn inspect_local_path(filename: &str) -> Result<PackagePreview, String> {
    validate_local_upload_path(filename)?;
    let value = inspect_local_upload(filename)?;
    host::validate_device_variant(&value.info.manifest)?;
    let installed = installed_versions(&value.info.manifest.id)
        .map(|versions| !versions.is_empty())
        .unwrap_or(false);
    Ok(PackagePreview {
        summary: summary_from_manifest(&value.info.manifest, installed),
        package_name: value.info.name.clone(),
        architecture: value.info.architecture.clone(),
        bundle: value.bundle,
        packages: local_package_contents(&value),
        preview_icon_data_url: value.info.preview_icon_data_url.clone(),
    })
}

fn local_package_contents(value: &InspectedUpload) -> Vec<LocalPackageContent> {
    let mut names: Vec<String> = value.packages.keys().cloned().collect();
    names.sort_by(|a, b| {
        let a_root = a == &value.info.name;
        let b_root = b == &value.info.name;
        b_root.cmp(&a_root).then_with(|| a.cmp(b))
    });
    names
        .into_iter()
        .map(|name| {
            let info = &value.packages[&name];
            let (installed_version, installed) = package_installed_version(&name);
            LocalPackageContent {
                name: name.clone(),
                version: info.version.clone(),
                architecture: info.architecture.clone(),
                root: name == value.info.name,
                installed,
                installed_version,
            }
        })
        .collect()
}

fn inspect_local_upload(filename: &str) -> Result<InspectedUpload, String> {
    let bundle = is_ipks(filename).map_err(|e| format!("inspect local extension upload: {e}"))?;
    let architectures = opkg_architectures()?;
    if !bundle {
        let info = inspect_ipk(filename, "", &architectures, None)?;
        let packages = std::collections::BTreeMap::from([(info.name.clone(), info.clone())]);
        return Ok(InspectedUpload {
            info,
            paths: vec![PathBuf::from(filename)],
            bundle: false,
            packages,
            cleanup: None,
        });
    }
    fs::create_dir_all(paths::OPKG_CACHE).map_err(|e| e.to_string())?;
    let directory = unique_temp_dir(paths::OPKG_CACHE, "local-ipks-")?;
    match inspect_ipks(filename, &directory, &architectures) {
        Ok(value) => Ok(InspectedUpload {
            info: value.root.clone(),
            paths: value.paths,
            bundle: true,
            packages: value.packages,
            cleanup: Some(directory),
        }),
        Err(err) => {
            let _ = fs::remove_dir_all(directory);
            Err(format!("reject extension bundle: {err}"))
        }
    }
}

pub fn install_package(package_name: &str) -> Result<(), String> {
    if !paths::valid_package_name(package_name) {
        return Err(
            "install accepts only an onekvm-extension-* package name, not a path or URL".into(),
        );
    }
    sys::with_lock(|| {
        crate::feed::refresh()?;
        fs::create_dir_all(paths::OPKG_CACHE).map_err(|e| e.to_string())?;
        let directory = unique_temp_dir(paths::OPKG_CACHE, "extension-")?;
        let result = (|| {
            sys::opkg_in(&directory, &["download", package_name])
                .map_err(|err| format!("download extension: {err}"))?;
            let matches: Vec<_> = fs::read_dir(&directory)
                .map_err(|e| e.to_string())?
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .and_then(|value| value.to_str())
                        == Some("ipk")
                })
                .map(|entry| entry.path())
                .collect();
            if matches.len() != 1 {
                return Err(format!("opkg downloaded {} IPK files", matches.len()));
            }
            install_path_unlocked(matches[0].to_str().unwrap_or_default(), package_name)
        })();
        let _ = fs::remove_dir_all(directory);
        result
    })
}

pub fn install_local(stdin: &mut dyn Read) -> Result<(), String> {
    fs::create_dir_all(paths::OPKG_CACHE).map_err(|e| e.to_string())?;
    let path = unique_temp_file(paths::OPKG_CACHE, "local-extension-")?;
    let mut file = fs::File::create(&path).map_err(|e| e.to_string())?;
    let written = io::copy(&mut stdin.take(paths::MAX_LOCAL_IPK + 1), &mut file)
        .map_err(|e| format!("read local extension package: {e}"))?;
    if written == 0 {
        let _ = fs::remove_file(&path);
        return Err("local extension package is empty".into());
    }
    if written > paths::MAX_LOCAL_IPK {
        let _ = fs::remove_file(&path);
        return Err("local extension package or bundle exceeds 32 MiB".into());
    }
    let result = sys::with_lock(|| install_upload_unlocked(path.to_str().unwrap_or_default()));
    let _ = fs::remove_file(&path);
    result
}

pub fn install_local_path(filename: &str) -> Result<(), String> {
    validate_local_upload_path(filename)?;
    sys::with_lock(|| install_upload_unlocked(filename))
}

fn install_upload_unlocked(filename: &str) -> Result<(), String> {
    let value = inspect_local_upload(filename)?;
    install_validated_unlocked(&value.info, &value.paths)
}

fn install_path_unlocked(filename: &str, expected_package: &str) -> Result<(), String> {
    let architectures = opkg_architectures()?;
    let info = inspect_ipk(filename, expected_package, &architectures, None)
        .map_err(|err| format!("reject extension package: {err}"))?;
    install_validated_unlocked(&info, &[PathBuf::from(filename)])
}

fn install_validated_unlocked(info: &PackageInfo, filenames: &[PathBuf]) -> Result<(), String> {
    host::validate_device_variant(&info.manifest)?;
    let state_exists = paths::state_path(&info.manifest.id).exists();
    let mut state = read_state(&info.manifest.id)?;
    if !state_exists && info.manifest.default_enabled {
        memory::validate(&info.manifest.id, &info.manifest)?;
    }
    let was_running = crate::sandbox::is_active(&paths::unit_name(&info.manifest.id));
    if was_running {
        lifecycle::disable_unlocked(&info.manifest.id)
            .map_err(|err| format!("stop extension before upgrade: {err}"))?;
    }
    let mut arguments = vec!["install".to_owned()];
    if filenames.len() > 1 {
        arguments.push("--force-reinstall".into());
    }
    for filename in filenames {
        arguments.push(filename.to_string_lossy().into_owned());
    }
    if let Err(err) = sys::opkg(&arguments.iter().map(String::as_str).collect::<Vec<_>>()) {
        if was_running {
            let _ = lifecycle::enable_unlocked(&info.manifest.id);
        }
        return Err(err);
    }
    if load_installed(&info.manifest.id, &info.manifest.version).is_err() {
        let _ = sys::opkg(&["remove", &info.name]);
        return Err("installed extension failed validation".into());
    }
    if !info.manifest.kernel_modules.is_empty() {
        sys::run(paths::DEPMOD, &["-a".into()]).map_err(|err| {
            format!("rebuild kernel module metadata: {err}")
        })?;
    }
    if state.active_version != info.manifest.version {
        state.active_version = info.manifest.version.clone();
        state.revision += 1;
    }
    if !state_exists && info.manifest.default_enabled {
        state.enabled = true;
    }
    write_state(&info.manifest.id, &state)?;
    if state.enabled {
        lifecycle::enable_unlocked(&info.manifest.id)?;
    }
    Ok(())
}

pub fn remove_package(package_name: &str) -> Result<(), String> {
    if !paths::valid_package_name(package_name) {
        return Err("remove accepts only an onekvm-extension-* package name".into());
    }
    let id = package_name.trim_start_matches("onekvm-extension-");
    sys::with_lock(|| {
        let mut state = read_state(id)?;
        if state.enabled {
            lifecycle::disable_unlocked(id)
                .map_err(|err| format!("disable extension before removal: {err}"))?;
        } else {
            let _ = crate::dbus_client::disable(id, state.revision);
        }
        sys::opkg(&["remove", package_name])?;
        state = read_state(id)?;
        state.enabled = false;
        state.active_version.clear();
        state.revision += 1;
        write_state(id, &state)
    })
}

pub fn purge(id: &str) -> Result<(), String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    sys::with_lock(|| {
        let state = read_state(id)?;
        if state.enabled {
            lifecycle::disable_unlocked(id)
                .map_err(|err| format!("disable extension before purge: {err}"))?;
        } else {
            let _ = crate::dbus_client::disable(id, state.revision);
        }
        let package_name = format!("onekvm-extension-{id}");
        if package_installed(&package_name) {
            sys::opkg(&["remove", &package_name])?;
        }
        for name in [
            Path::new(paths::APPS_ROOT).join(id),
            Path::new(paths::APP_DATA_ROOT).join(id),
            Path::new(paths::RUNTIME_ROOT).join(id),
        ] {
            if let Err(err) = fs::remove_dir_all(name) {
                if err.kind() != io::ErrorKind::NotFound {
                    return Err(err.to_string());
                }
            }
        }
        match fs::remove_file(paths::state_path(id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    })
}

fn package_installed(name: &str) -> bool {
    package_installed_version(name).1
}

fn package_installed_version(name: &str) -> (String, bool) {
    let Ok(output) = sys::opkg_output(&["status", name]) else {
        return (String::new(), false);
    };
    let Ok(fields) = parse_control(&output) else {
        return (String::new(), false);
    };
    if fields
        .get("Status")
        .is_some_and(|status| status.ends_with(" installed"))
    {
        (
            fields.get("Version").cloned().unwrap_or_default(),
            true,
        )
    } else {
        (String::new(), false)
    }
}

fn opkg_architectures() -> Result<BTreeSet<String>, String> {
    let output = sys::opkg_output(&["print-architecture"])?;
    let mut result = BTreeSet::new();
    for line in String::from_utf8_lossy(&output).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 && fields[0] == "arch" {
            result.insert(fields[1].to_owned());
        }
    }
    if result.is_empty() {
        return Err("opkg returned no accepted architectures".into());
    }
    Ok(result)
}

fn unique_temp_dir(root: &str, prefix: &str) -> Result<PathBuf, String> {
    for n in 0..1000 {
        let path = Path::new(root).join(format!("{prefix}{}-{n}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        }
    }
    Err("create temporary directory".into())
}

fn unique_temp_file(root: &str, prefix: &str) -> Result<PathBuf, String> {
    for n in 0..1000 {
        let path = Path::new(root).join(format!("{prefix}{}-{n}.upload", std::process::id()));
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        }
    }
    Err("create temporary file".into())
}

#[cfg(test)]
mod tests {
    use super::validate_local_upload_path;

    #[test]
    fn rejects_paths_outside_staging() {
        assert!(validate_local_upload_path("/tmp/evil.ipk").is_err());
        assert!(validate_local_upload_path("/var/tmp/onekvm/extension-uploads/../x.upload").is_err());
    }
}
