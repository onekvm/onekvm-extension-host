use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::catalog::{summary_from_manifest, CatalogSummary};
use crate::control::parse_control;
use crate::host;
use crate::ipk::{inspect_ipk, PackageInfo};
use crate::ipks::{inspect_ipks, is_ipks};
use crate::lifecycle;
use crate::manifest::{installed_versions, load_installed};
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
    let info =
        fs::symlink_metadata(&clean).map_err(|e| format!("read staged extension package: {e}"))?;
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
        summary: summary_from_manifest(
            &value.info.manifest,
            installed,
            value.info.preview_icon_data_url.clone(),
        ),
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

pub fn resolve_extension_package(name: &str) -> Result<String, String> {
    if paths::valid_package_name(name) {
        return Ok(name.to_owned());
    }
    if paths::valid_id(name) {
        return Ok(format!("onekvm-extension-{name}"));
    }
    Err("install accepts an extension id or onekvm-extension-* package name".into())
}

#[derive(Clone)]
struct InstallProgress {
    active: bool,
    package: String,
    phase: String,
    percent: Option<u8>,
    bytes: u64,
    total_bytes: u64,
    message: String,
    error: String,
    log: String,
    log_truncated: bool,
}

impl InstallProgress {
    fn idle() -> Self {
        Self {
            active: false,
            package: String::new(),
            phase: "idle".into(),
            percent: None,
            bytes: 0,
            total_bytes: 0,
            message: String::new(),
            error: String::new(),
            log: String::new(),
            log_truncated: false,
        }
    }
}

fn progress_slot() -> &'static Mutex<InstallProgress> {
    static SLOT: OnceLock<Mutex<InstallProgress>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(InstallProgress::idle()))
}

fn job_slot() -> &'static Mutex<Option<thread::JoinHandle<Result<(), String>>>> {
    static SLOT: OnceLock<Mutex<Option<thread::JoinHandle<Result<(), String>>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn lock_progress() -> std::sync::MutexGuard<'static, InstallProgress> {
    progress_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const MAX_INSTALL_LOG_BYTES: usize = 64 * 1024;

fn append_log(progress: &mut InstallProgress, text: &str) {
    if text.is_empty() {
        return;
    }
    progress.log.push_str(text.trim_end_matches(['\r', '\n']));
    progress.log.push('\n');
    if progress.log.len() > MAX_INSTALL_LOG_BYTES {
        let mut start = progress.log.len() - MAX_INSTALL_LOG_BYTES;
        while !progress.log.is_char_boundary(start) {
            start += 1;
        }
        progress.log.drain(..start);
        progress.log_truncated = true;
    }
}

pub(crate) fn log_install_message(text: &str) {
    let mut progress = lock_progress();
    if progress.active {
        append_log(&mut progress, text);
    }
}

fn set_progress(
    phase: &str,
    percent: Option<u8>,
    bytes: u64,
    total_bytes: u64,
    message: impl Into<String>,
) {
    let mut progress = lock_progress();
    let message = message.into();
    if !message.is_empty() && (progress.phase != phase || progress.message != message) {
        append_log(&mut progress, &format!("[{phase}] {message}"));
    }
    progress.phase = phase.to_owned();
    progress.percent = percent;
    progress.bytes = bytes;
    if total_bytes > 0 {
        progress.total_bytes = total_bytes;
    }
    progress.message = message;
}

fn download_percent(bytes: u64, total: u64) -> Option<u8> {
    if total == 0 {
        return None;
    }
    Some(bytes.saturating_mul(100).saturating_div(total).min(99) as u8)
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.metadata().ok())
        .filter(|info| info.is_file())
        .map(|info| info.len())
        .sum()
}

fn download_bytes(cache: &Path, tmp: &Path) -> u64 {
    directory_size(cache).saturating_add(directory_size(tmp))
}

fn parse_output_percent(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut found = None;
    while index < bytes.len() {
        if bytes[index] == b'%' && index > 0 {
            let mut start = index;
            while start > 0 && bytes[start - 1].is_ascii_digit() {
                start -= 1;
            }
            if start < index {
                if let Some(value) = std::str::from_utf8(&bytes[start..index])
                    .ok()
                    .and_then(|digits| digits.parse::<u16>().ok())
                {
                    if value <= 100 {
                        found = Some(value as u8);
                    }
                }
            }
        }
        index += 1;
    }
    found
}

fn parse_content_length(text: &str) -> Option<u64> {
    let rest = text
        .split_once("Length:")
        .or_else(|| text.split_once("length:"))?
        .1
        .trim_start();
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn count_ipks(path: &Path) -> u32 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("ipk"))
        .count() as u32
}

pub fn install_progress() -> Value {
    let progress = lock_progress().clone();
    json!({
        "active": progress.active,
        "package": progress.package,
        "phase": progress.phase,
        "percent": progress.percent,
        "bytes": progress.bytes,
        "total_bytes": progress.total_bytes,
        "message": progress.message,
        "error": progress.error,
        "log": progress.log,
        "log_truncated": progress.log_truncated,
        "status": if progress.phase == "done" {
            "ok"
        } else if progress.phase == "error" {
            "error"
        } else if progress.active {
            "running"
        } else {
            "idle"
        },
    })
}

pub fn install_package(name: &str) -> Result<(), String> {
    install_package_with_options(name, true, 0)
}

pub fn install_package_with_options(name: &str, wait: bool, size_bytes: u64) -> Result<(), String> {
    let package_name = resolve_extension_package(name)?;
    start_install_job(&package_name, size_bytes)?;
    if wait {
        join_install_job()
    } else {
        Ok(())
    }
}

fn start_install_job(package_name: &str, size_bytes: u64) -> Result<(), String> {
    let mut job = job_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(handle) = job.take() {
        if !handle.is_finished() {
            *job = Some(handle);
            return Err("another plugin install is already running".into());
        }
        let _ = handle.join();
    }
    {
        let mut progress = lock_progress();
        if progress.active {
            return Err("another plugin install is already running".into());
        }
        *progress = InstallProgress {
            active: true,
            package: package_name.to_owned(),
            phase: "prepare".into(),
            percent: None,
            bytes: 0,
            total_bytes: size_bytes,
            message: "Refreshing package lists".into(),
            error: String::new(),
            log: "[prepare] Refreshing package lists\n".into(),
            log_truncated: false,
        };
    }
    let package = package_name.to_owned();
    *job = Some(thread::spawn(move || {
        let result = install_package_job(&package, size_bytes);
        let mut progress = lock_progress();
        progress.active = false;
        match &result {
            Ok(()) => {
                append_log(&mut progress, "[done] Installation complete");
                progress.phase = "done".into();
                progress.percent = Some(100);
                progress.message.clear();
                progress.error.clear();
            }
            Err(err) => {
                append_log(&mut progress, &format!("[error] {err}"));
                progress.phase = "error".into();
                progress.error = err.clone();
            }
        }
        result
    }));
    Ok(())
}

fn join_install_job() -> Result<(), String> {
    let handle = job_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .ok_or_else(|| "install job missing".to_string())?;
    handle
        .join()
        .unwrap_or_else(|_| Err("install thread panicked".into()))
}

fn install_package_job(package_name: &str, size_bytes: u64) -> Result<(), String> {
    sys::with_lock(|| {
        crate::transaction::recover_all(true)?;
        set_progress("prepare", None, 0, size_bytes, "Refreshing package lists");
        crate::feed::refresh_if_stale()?;
        fs::create_dir_all(paths::OPKG_CACHE).map_err(|e| e.to_string())?;
        let cache = unique_temp_dir(paths::OPKG_CACHE, "ext-cache-")?;
        let tmp = unique_temp_dir(paths::OPKG_CACHE, "ext-tmp-")?;
        let result = (|| {
            set_progress(
                "download",
                download_percent(0, size_bytes),
                0,
                size_bytes,
                "Downloading packages",
            );
            run_opkg_progress(
                &tmp,
                &cache,
                &["--download-only", "install", package_name],
                "download",
                size_bytes,
            )?;
            let downloaded = directory_size(&cache);
            set_progress(
                "download",
                Some(100),
                downloaded,
                size_bytes.max(downloaded),
                "Download complete",
            );
            log_install_message("Verifying downloaded package and manifest");
            let info = inspect_cached_extension(&cache, package_name)?;
            log_install_message("Saving previous extension state");
            let prep = prepare_installed_upgrade(&info)?;
            set_progress(
                "install",
                Some(0),
                downloaded,
                size_bytes.max(downloaded),
                "Installing packages",
            );
            if let Err(err) =
                run_opkg_progress(&tmp, &cache, &["install", package_name], "install", 0)
            {
                return crate::transaction::failed(&prep.snapshot, err);
            }
            commit_installed_package(&info, prep)
        })();
        let _ = fs::remove_dir_all(&cache);
        let _ = fs::remove_dir_all(&tmp);
        result
    })
}

fn inspect_cached_extension(cache: &Path, package_name: &str) -> Result<PackageInfo, String> {
    let architectures = opkg_architectures()?;
    let mut last_err = "downloaded package not found in cache".to_string();
    let Ok(entries) = fs::read_dir(cache) else {
        return Err(last_err);
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("ipk") {
            continue;
        }
        match inspect_ipk(path.to_str().unwrap_or_default(), "", &architectures, None) {
            Ok(info) if info.name == package_name => return Ok(info),
            Ok(_) => continue,
            Err(err) => last_err = err,
        }
    }
    Err(format!("reject extension package: {last_err}"))
}

pub(crate) fn run_opkg_progress(
    tmp: &Path,
    cache: &Path,
    args: &[&str],
    phase: &str,
    total_bytes: u64,
) -> Result<(), String> {
    fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    let mut command = Command::new(paths::OPKG);
    command
        .arg(if phase == "download" { "-V2" } else { "-V1" })
        .arg("--tmp-dir")
        .arg(tmp)
        .arg("--cache-dir")
        .arg(cache)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("opkg: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let watcher = if phase == "download" {
        let stop = stop.clone();
        let cache = cache.to_path_buf();
        let tmp = tmp.to_path_buf();
        Some(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let bytes = download_bytes(&cache, &tmp);
                let current = lock_progress();
                let known_total = current.total_bytes.max(total_bytes);
                let percent = match (download_percent(bytes, known_total), current.percent) {
                    (Some(from_bytes), Some(from_output)) => Some(from_bytes.max(from_output)),
                    (Some(from_bytes), None) => Some(from_bytes),
                    (None, from_output) => from_output,
                };
                let message = current.message.clone();
                let reported_bytes = bytes.max(current.bytes);
                drop(current);
                set_progress("download", percent, reported_bytes, known_total, message);
                thread::sleep(Duration::from_millis(200));
            }
        }))
    } else {
        None
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let mut readers = Vec::new();
    if let Some(pipe) = stdout {
        let tx = tx.clone();
        readers.push(thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    if let Some(pipe) = stderr {
        let tx = tx.clone();
        readers.push(thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx);
    let mut installed = 0u32;
    let mut last_output = String::new();
    let packages = count_ipks(cache).max(1);
    for line in rx {
        if !line.trim().is_empty() {
            last_output = line.trim().to_owned();
        }
        log_install_message(&line);
        apply_opkg_line(phase, &line, &mut installed, packages, total_bytes);
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait().map_err(|e| format!("opkg: {e}"))?;
    stop.store(true, Ordering::Relaxed);
    if let Some(watcher) = watcher {
        let _ = watcher.join();
    }
    if status.success() {
        Ok(())
    } else {
        let detail = last_output;
        if detail.is_empty() {
            Err(format!("opkg failed with {}", status.code().unwrap_or(-1)))
        } else {
            Err(format!(
                "opkg failed with {}: {detail}",
                status.code().unwrap_or(-1)
            ))
        }
    }
}

fn current_progress_message() -> String {
    lock_progress().message.clone()
}

fn apply_opkg_line(phase: &str, line: &str, installed: &mut u32, packages: u32, total_bytes: u64) {
    let text = line.trim();
    if text.is_empty() {
        return;
    }
    if phase == "download" {
        if let Some(length) = parse_content_length(text) {
            let bytes = lock_progress().bytes;
            set_progress(
                "download",
                download_percent(bytes, length.max(total_bytes)),
                bytes,
                length.max(total_bytes),
                current_progress_message(),
            );
        }
        if let Some(percent) = parse_output_percent(text) {
            let current = lock_progress();
            let bytes = current.bytes;
            let known_total = current.total_bytes.max(total_bytes);
            let message = current.message.clone();
            drop(current);
            set_progress(
                "download",
                Some(percent.min(99)),
                bytes,
                known_total,
                message,
            );
            return;
        }
    }
    if let Some(rest) = text.strip_prefix("Downloading ") {
        let file = rest
            .trim_end_matches('.')
            .rsplit('/')
            .next()
            .unwrap_or(rest)
            .to_owned();
        let bytes = lock_progress().bytes;
        if phase == "download" {
            set_progress(
                "download",
                download_percent(bytes, total_bytes),
                bytes,
                total_bytes,
                format!("Downloading {file}"),
            );
        } else {
            let percent = lock_progress().percent;
            set_progress(phase, percent, bytes, 0, format!("Downloading {file}"));
        }
        return;
    }
    if let Some(rest) = text.strip_prefix("Installing ") {
        *installed = installed.saturating_add(1);
        let name = rest.split_whitespace().next().unwrap_or(rest);
        let percent = ((*installed as u64) * 100 / u64::from(packages.max(1))).min(99) as u8;
        let bytes = lock_progress().bytes;
        set_progress(
            "install",
            Some(percent),
            bytes,
            0,
            format!("Installing {name}"),
        );
        return;
    }
    if let Some(rest) = text.strip_prefix("Configuring ") {
        let name = rest.trim_end_matches('.').trim();
        let bytes = lock_progress().bytes;
        set_progress(
            "install",
            Some(100),
            bytes,
            0,
            format!("Configuring {name}"),
        );
    }
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

struct UpgradePrep {
    snapshot: crate::transaction::Snapshot,
    state_exists: bool,
    state: crate::state::PluginState,
}

fn prepare_installed_upgrade(info: &PackageInfo) -> Result<UpgradePrep, String> {
    host::validate_device_variant(&info.manifest)?;
    let state_exists = paths::state_path(&info.manifest.id).exists();
    let state = read_state(&info.manifest.id)?;
    if !state_exists && crate::state::starts_enabled(&info.manifest) {
        memory::validate(&info.manifest.id, &info.manifest)?;
    }
    let was_running = state.enabled
        || crate::sandbox::is_active(&paths::unit_name(&info.manifest.id))
        || crate::sandbox::is_active(&paths::helper_unit(&info.manifest.id));
    let mut snapshot = crate::transaction::Snapshot::begin(&info.manifest.id)?;
    let preparation = (|| {
        if was_running {
            lifecycle::disable_unlocked(&info.manifest.id)?;
        }
        host::stop_declared_service_units(&info.manifest, false)?;
        host::stop_extension_runtime(&info.manifest.id)?;
        snapshot.capture()
    })();
    if let Err(error) = preparation {
        crate::transaction::failed(&snapshot, format!("prepare extension upgrade: {error}"))?;
    }

    Ok(UpgradePrep {
        snapshot,
        state_exists,
        state,
    })
}

fn commit_installed_package(info: &PackageInfo, prep: UpgradePrep) -> Result<(), String> {
    let result = (|| {
        log_install_message("Validating installed extension");
        load_installed(&info.manifest.id, &info.manifest.version)
            .map_err(|e| format!("installed extension failed validation: {e}"))?;
        if !info.manifest.kernel_modules.is_empty() {
            sys::run(paths::DEPMOD, &["-a".into()])?;
        }
        let mut state = prep.state.clone();
        if state.active_version != info.manifest.version {
            state.active_version = info.manifest.version.clone();
            state.revision += 1;
        }
        if !prep.state_exists && crate::state::starts_enabled(&info.manifest) {
            state.enabled = true;
        }
        crate::state::normalize_environment(&info.manifest, &mut state);
        write_state(&info.manifest.id, &state)?;
        if state.enabled {
            log_install_message("Activating extension");
            lifecycle::enable_unlocked(&info.manifest.id)?;
            log_install_message("Checking extension startup");
            lifecycle::check_started(&info.manifest.id)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => prep.snapshot.commit(),
        Err(error) => crate::transaction::failed(&prep.snapshot, error),
    }
}

fn install_validated_unlocked(info: &PackageInfo, filenames: &[PathBuf]) -> Result<(), String> {
    let prep = prepare_installed_upgrade(info)?;
    let mut arguments = vec!["--force-reinstall".to_owned(), "install".to_owned()];
    for filename in filenames {
        arguments.push(filename.to_string_lossy().into_owned());
    }
    if let Err(err) = sys::opkg(&arguments.iter().map(String::as_str).collect::<Vec<_>>()) {
        return crate::transaction::failed(&prep.snapshot, err);
    }
    commit_installed_package(info, prep)
}

fn stop_before_removal(id: &str, state: &crate::state::PluginState) -> Result<(), String> {
    let helper_active = crate::sandbox::is_active(&paths::helper_unit(id));
    let legacy_active = crate::sandbox::is_active(&paths::unit_name(id));
    if state.enabled {
        lifecycle::disable_unlocked(id)
            .map_err(|err| format!("stop extension before removal: {err}"))?;
    } else if helper_active || legacy_active {
        crate::dbus_client::disable(id, state.revision)
            .map_err(|err| format!("stop extension before removal: {err}"))?;
    } else {
        let _ = crate::dbus_client::disable(id, state.revision);
    }
    if let Ok(extension) =
        load_installed(id, &state.active_version).or_else(|_| load_installed(id, ""))
    {
        host::stop_declared_service_units(&extension.manifest, true)
            .map_err(|err| format!("stop extension service before removal: {err}"))?;
    }
    host::stop_extension_runtime(id)
        .map_err(|err| format!("stop extension runtime before removal: {err}"))
}

pub fn remove_package(name: &str) -> Result<(), String> {
    let package_name = resolve_extension_package(name)
        .map_err(|_| "remove accepts an extension id or onekvm-extension-* package name")?;
    let id = package_name.trim_start_matches("onekvm-extension-");
    sys::with_lock(|| {
        crate::transaction::recover_all(true)?;
        let mut state = read_state(id)?;
        stop_before_removal(id, &state)?;
        sys::opkg(&["remove", &package_name])?;
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
        crate::transaction::recover_all(true)?;
        let state = read_state(id)?;
        stop_before_removal(id, &state)?;
        let package_name = format!("onekvm-extension-{id}");
        if package_installed(&package_name) {
            sys::opkg(&["remove", &package_name])?;
        }
        for name in [
            Path::new(paths::APPS_ROOT).join(id),
            Path::new(paths::APP_DATA_ROOT).join(id),
            Path::new(paths::RUNTIME_ROOT).join(id),
        ] {
            if let Err(err) = remove_extension_tree(&name) {
                if err.kind() != io::ErrorKind::NotFound {
                    return Err(err.to_string());
                }
            }
        }
        match fs::remove_file(paths::state_path(id)) {
            Ok(()) => crate::contributions::changed(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => crate::contributions::changed(),
            Err(err) => Err(err.to_string()),
        }
    })
}

// Core's CLI children have CAP_CHOWN, but deliberately lack DAC_OVERRIDE.
// Stopped extensions own their private data directories. Take directory
// ownership before removing them; unlink symlinks without visiting targets.
pub(crate) fn remove_extension_tree(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    let owner = unsafe { libc::geteuid() };
    if metadata.uid() != owner {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        if unsafe { libc::lchown(name.as_ptr(), owner, u32::MAX) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() | 0o700))?;
    for entry in fs::read_dir(path)? {
        remove_extension_tree(&entry?.path())?;
    }
    fs::remove_dir(path)
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
        (fields.get("Version").cloned().unwrap_or_default(), true)
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
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        }
    }
    Err("create temporary file".into())
}

#[cfg(test)]
mod tests {
    use super::{
        download_percent, parse_content_length, parse_output_percent, validate_local_upload_path,
    };

    #[test]
    fn installation_log_keeps_a_bounded_utf8_tail_and_final_failure() {
        let mut progress = super::InstallProgress::idle();
        super::append_log(&mut progress, &"正在下载\n".repeat(10000));
        super::append_log(
            &mut progress,
            "health check failed; previous extension restored",
        );
        assert!(progress.log_truncated);
        assert!(progress.log.len() <= super::MAX_INSTALL_LOG_BYTES);
        assert!(progress
            .log
            .ends_with("health check failed; previous extension restored\n"));
        assert!(progress.log.contains("正在下载"));
        let reset = super::InstallProgress::idle();
        assert!(reset.log.is_empty());
        assert!(!reset.log_truncated);
    }

    #[test]
    fn purge_cleans_private_directories_without_following_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("onekvm-purge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data/private")).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/keep"), b"preserve").unwrap();
        std::fs::write(root.join("data/private/script"), b"REM").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("data/link")).unwrap();
        std::fs::set_permissions(
            root.join("data/private"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        super::remove_extension_tree(&root.join("data")).unwrap();
        assert!(!root.join("data").exists());
        assert_eq!(
            std::fs::read(root.join("outside/keep")).unwrap(),
            b"preserve"
        );
        super::remove_extension_tree(&root.join("missing")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_paths_outside_staging() {
        assert!(validate_local_upload_path("/tmp/evil.ipk").is_err());
        assert!(
            validate_local_upload_path("/var/tmp/onekvm/extension-uploads/../x.upload").is_err()
        );
    }

    #[test]
    fn download_percent_caps_before_completion() {
        assert_eq!(download_percent(0, 100), Some(0));
        assert_eq!(download_percent(50, 100), Some(50));
        assert_eq!(download_percent(100, 100), Some(99));
        assert_eq!(download_percent(10, 0), None);
    }

    #[test]
    fn parses_wget_and_opkg_percent_tokens() {
        assert_eq!(parse_output_percent("  45% |****      |  234k"), Some(45));
        assert_eq!(
            parse_output_percent("foo.ipk              12% |***         |  12345"),
            Some(12)
        );
        assert_eq!(parse_output_percent("Downloading package"), None);
        assert_eq!(parse_output_percent("101% complete"), None);
    }

    #[test]
    fn parses_wget_content_length() {
        assert_eq!(
            parse_content_length("Length: 1234567 (1.1M)"),
            Some(1_234_567)
        );
        assert_eq!(parse_content_length("length: 80"), Some(80));
        assert_eq!(parse_content_length("Downloading foo.ipk"), None);
    }
}
