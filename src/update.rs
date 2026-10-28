//! Kernel-change upgrade preparation and offline installation before Core starts.
//! Never resolve target-kernel dependencies against the running kernel's packages.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{control, ipk, ipks, kernel_identity, manifest, paths, state, sys};

#[path = "update_retained.rs"]
mod retained;

const ROOT: &str = "/data/rauc/plugin-update";
const SLOTS: &str = "/data/onekvm/plugin-slots";
const MAX_PACKAGES: usize = 128;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Package {
    version: String,
    #[serde(default)]
    extension_version: String,
    #[serde(default)]
    depends: String,
    #[serde(default)]
    provides: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    extension: Option<manifest::Manifest>,
    #[serde(default)]
    modules: BTreeSet<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Plugin {
    id: String,
    name: String,
    version: String,
    depends: Option<String>,
    kernel_modules: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Plan {
    id: String,
    phase: String,
    bundle: String,
    bundle_sha256: String,
    current_kernel: String,
    target_kernel: String,
    #[serde(default)]
    current_kernel_magic: String,
    #[serde(default)]
    target_kernel_magic: String,
    source_build: String,
    target_build: String,
    source_slot: String,
    target_slot: String,
    baseline: BTreeMap<String, Package>,
    retained: BTreeMap<String, Package>,
    packages: BTreeMap<String, Package>,
    plugins: Vec<Plugin>,
    target_modules: BTreeSet<String>,
    #[serde(default)]
    error: String,
}

#[derive(Serialize, Deserialize)]
struct Slot {
    build: String,
    kernel: String,
    #[serde(default)]
    kernel_magic: String,
    directory: String,
    #[serde(default)]
    plugins: Vec<String>,
}

fn root() -> PathBuf {
    PathBuf::from(ROOT)
}
fn text(path: impl AsRef<Path>) -> Result<String, String> {
    fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| e.to_string())
}
fn kernel() -> Result<String, String> {
    text("/proc/sys/kernel/osrelease")
}
fn build() -> Result<String, String> {
    text("/usr/share/onekvm/build-id")
}
fn slot() -> Result<String, String> {
    let value = text("/run/onekvm/slot")?;
    if !matches!(value.as_str(), "A" | "B") {
        return Err("plugin upgrades require an A/B boot slot".into());
    }
    Ok(value)
}
fn output(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    sys::output(
        program,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}
fn read_json<T: serde::de::DeserializeOwned>(path: impl AsRef<Path>) -> Result<T, String> {
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
fn write_json(path: impl AsRef<Path>, value: &impl Serialize) -> Result<(), String> {
    sys::atomic_write(
        path.as_ref(),
        &serde_json::to_vec(value).map_err(|e| e.to_string())?,
        0o600,
    )
}
fn load() -> Result<Plan, String> {
    read_json(root().join("plan.json"))
}
fn save(plan: &Plan) -> Result<(), String> {
    write_json(root().join("plan.json"), plan)
}
fn hash(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    output(
        "cp",
        &[
            "-a",
            &format!("{}/.", from.display()),
            &to.to_string_lossy(),
        ],
    )?;
    Ok(())
}
fn remove_tree(path: impl AsRef<Path>) -> Result<(), String> {
    // Snapshots retain extension-owned directories. Core has CAP_CHOWN but
    // no DAC_OVERRIDE, so use the same symlink-safe cleanup as plugin removal.
    crate::install::remove_extension_tree(path.as_ref()).map_err(|e| e.to_string())
}
fn package(fields: &BTreeMap<String, String>) -> Package {
    Package {
        version: fields.get("Version").cloned().unwrap_or_default(),
        extension_version: fields
            .get("X-OneKVM-Extension-Version")
            .cloned()
            .unwrap_or_default(),
        depends: fields.get("Depends").cloned().unwrap_or_default(),
        provides: fields.get("Provides").cloned().unwrap_or_default(),
        ..Package::default()
    }
}
fn database(data: &[u8]) -> Result<BTreeMap<String, Package>, String> {
    let mut result = BTreeMap::new();
    for part in String::from_utf8_lossy(data)
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
    {
        let fields = control::parse_control(part.as_bytes())?;
        if fields
            .get("Status")
            .is_some_and(|s| !s.ends_with(" installed"))
        {
            continue;
        }
        if let Some(name) = fields.get("Package") {
            result.insert(name.clone(), package(&fields));
        }
    }
    Ok(result)
}
fn inventory() -> Result<Vec<Plugin>, String> {
    let mut plugins = Vec::new();
    for entry in fs::read_dir(paths::APPS_ROOT).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !paths::valid_id(&id) || !entry.path().is_dir() {
            continue;
        }
        let state = state::read_state(&id)?;
        let installed = manifest::load_installed(&id, &state.active_version)?;
        let m = installed.manifest;
        let deps =
            control::installed_package_dependencies(&format!("onekvm-extension-{id}"), &m.version);
        plugins.push(Plugin {
            id,
            name: m.name,
            version: m.version,
            depends: deps.map(|d| d.join(", ")),
            kernel_modules: m.kernel_modules,
        });
    }
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(plugins)
}

fn version_matches(version: &str, compare: &str, required: &str) -> bool {
    let ordering = control::compare_versions(version, required);
    match compare {
        "" => true,
        "=" => ordering.is_eq(),
        ">=" => !ordering.is_lt(),
        "<=" => !ordering.is_gt(),
        ">" | ">>" => ordering.is_gt(),
        "<" | "<<" => ordering.is_lt(),
        _ => false,
    }
}
fn is_kernel_package(name: &str) -> bool {
    name.starts_with("onekvm-kernel-module-")
        || name == "onekvm-kernel-modules-base"
        || name.starts_with("kernel-module-")
        || name.starts_with("kernel-image")
}
fn available<'a>(plan: &'a Plan, name: &str) -> Option<&'a Package> {
    plan.packages
        .get(name)
        .or_else(|| plan.baseline.get(name))
        .or_else(|| {
            (!is_kernel_package(name))
                .then(|| plan.retained.get(name))
                .flatten()
        })
}
fn missing(plan: &Plan) -> Vec<Value> {
    if !kernel_identity::valid_magic(&plan.target_kernel_magic) {
        return vec![
            json!({"package": "target-kernel-metadata", "reason": "kernel_metadata_missing"}),
        ];
    }
    if !kernel_changed(plan) {
        return Vec::new();
    }
    let mut missing = BTreeMap::new();
    let mut modules = plan.target_modules.clone();
    for value in plan.packages.values() {
        modules.extend(value.modules.iter().cloned());
    }
    let mut queue = Vec::new();
    for plugin in &plan.plugins {
        let name = format!("onekvm-extension-{}", plugin.id);
        if let Some(package) = plan
            .packages
            .get(&name)
            .or_else(|| plan.baseline.get(&name))
        {
            queue.push((plugin.id.clone(), name, package.depends.clone()));
        } else if let Some(depends) = &plugin.depends {
            queue.push((plugin.id.clone(), name, depends.clone()));
        } else {
            missing.insert(
                name.clone(),
                json!({"package": name, "plugin": plugin.id, "reason": "metadata_missing"}),
            );
        }
        let required = plan
            .packages
            .get(&format!("onekvm-extension-{}", plugin.id))
            .or_else(|| {
                plan.baseline
                    .get(&format!("onekvm-extension-{}", plugin.id))
            })
            .and_then(|p| p.extension.as_ref())
            .map(|m| &m.kernel_modules)
            .unwrap_or(&plugin.kernel_modules);
        for module in required {
            if !modules.contains(&module.replace('-', "_")) {
                let name = format!("kernel-module-{module}");
                missing.insert(name.clone(), json!({"package": name, "plugin": plugin.id, "reason": "kernel_module_missing"}));
            }
        }
    }
    let mut visited = BTreeSet::new();
    while let Some((plugin, parent, depends)) = queue.pop() {
        if !visited.insert(parent.clone()) {
            continue;
        }
        for expression in depends.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let mut found = None;
            for alternative in expression.split('|').map(str::trim) {
                let Ok(parsed) = ipk::parse_package_dependencies(alternative) else {
                    continue;
                };
                let Some(dep) = parsed.first() else {
                    continue;
                };
                let candidate = available(plan, &dep.name).or_else(|| {
                    plan.packages
                        .values()
                        .chain(plan.baseline.values())
                        .find(|p| p.provides.split(',').any(|s| s.trim() == dep.name))
                });
                if let Some(value) =
                    candidate.filter(|p| version_matches(&p.version, &dep.compare, &dep.version))
                {
                    found = Some((dep.name.clone(), value));
                    break;
                }
            }
            match found {
                Some((name, value)) if !plan.baseline.contains_key(&name) => {
                    queue.push((plugin.clone(), name, value.depends.clone()))
                }
                Some(_) => (),
                None => {
                    missing.insert(expression.to_owned(), json!({"package": expression, "plugin": plugin, "reason": "dependency_missing"}));
                }
            }
        }
    }
    missing.into_values().collect()
}
fn kernel_changed(plan: &Plan) -> bool {
    kernel_identity::changed(
        &plan.current_kernel,
        &plan.current_kernel_magic,
        &plan.target_kernel,
        &plan.target_kernel_magic,
    )
}
fn view(plan: &Plan) -> Value {
    let missing = missing(plan);
    json!({
        "id": plan.id, "phase": plan.phase,
        "current_kernel": plan.current_kernel, "target_kernel": plan.target_kernel,
        "current_kernel_magic": plan.current_kernel_magic,
        "target_kernel_magic": plan.target_kernel_magic,
        "kernel_changed": kernel_changed(plan),
        "target_build": plan.target_build, "missing": missing,
        "error": plan.error,
        "ready": missing.is_empty() && plan.phase == "prepared",
        "packages": plan.packages.iter().map(|(name,p)| json!({"name": name, "version": p.version})).collect::<Vec<_>>(),
    })
}

fn squash(image: &Path, path: &str) -> Result<Vec<u8>, String> {
    output(
        "unsquashfs",
        &[
            "-mem",
            "16M",
            "-processors",
            "1",
            "-cat",
            &image.to_string_lossy(),
            path,
        ],
    )
}
fn module_control(data: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut control_data = None;
    for entry in tar::Archive::new(decoder)
        .entries()
        .map_err(|e| e.to_string())?
    {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if entry.size() > 65536 {
            return Err("kernel control entry is too large".into());
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        match name {
            "control" => control_data = Some(bytes),
            "postinst" | "postrm" => {
                let script = String::from_utf8_lossy(&bytes)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let script = script.strip_prefix("#!/bin/sh ").unwrap_or(&script);
                let script = script.strip_prefix("set -e ").unwrap_or(script);
                if script != "if [ -z \"$D\" ]; then depmod -a fi" {
                    return Err("kernel package contains an unsupported maintenance script".into());
                }
            }
            "preinst" | "prerm" => {
                return Err("kernel package contains an unsupported maintenance script".into())
            }
            _ => (),
        }
    }
    control::parse_control(&control_data.ok_or("kernel package control is missing")?)
}

fn verify_module_release(data: &[u8], target: &str) -> Result<(), String> {
    let release = data
        .windows(9)
        .position(|w| w == b"vermagic=")
        .and_then(|offset| data.get(offset + 9..))
        .and_then(|value| value.split(|b| *b == 0 || *b == b' ').next())
        .and_then(|value| std::str::from_utf8(value).ok());
    if release != Some(target) {
        return Err(format!(
            "kernel module vermagic {:?} does not match target {target}",
            release
        ));
    }
    Ok(())
}
fn module_names(list: &[u8]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut releases = BTreeSet::new();
    let mut modules = BTreeSet::new();
    for line in String::from_utf8_lossy(list).lines() {
        if let Some((_, tail)) = line.split_once("/lib/modules/") {
            if let Some(release) = tail.split('/').next().filter(|s| !s.is_empty()) {
                releases.insert(release.into());
            }
        }
        if let Some(name) = line
            .split_whitespace()
            .last()
            .and_then(|s| s.rsplit('/').next())
            .and_then(|s| s.strip_suffix(".ko"))
        {
            modules.insert(name.replace('-', "_"));
        }
    }
    (releases, modules)
}
fn prepare(filename: &str) -> Result<Value, String> {
    if load().is_ok_and(|p| p.phase == "armed" || p.phase == "installing") {
        return Err("a kernel upgrade is already pending; reboot or finish it first".into());
    }
    let filename = fs::canonicalize(filename).map_err(|e| e.to_string())?;
    if filename.parent() != Some(Path::new("/data/rauc/incoming"))
        || !fs::metadata(&filename).is_ok_and(|m| m.is_file())
    {
        return Err("update bundle must be staged in /data/rauc/incoming".into());
    }
    if root().exists() {
        remove_tree(root())?;
    }
    fs::create_dir_all(root()).map_err(|e| e.to_string())?;
    fs::set_permissions(root(), fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let unpack = root().join("bundle");
    // RAUC verifies the signature and verity payload before any metadata is trusted.
    // RAUC extract launches unsquashfs with its default 512 MiB cache, which
    // cannot run on NanoKVM. Bound only this child's extractor; leave the
    // system executable and RAUC's signature/verity checks intact.
    let tools = root().join("tools");
    fs::create_dir_all(&tools).map_err(|e| e.to_string())?;
    sys::atomic_write(
        &tools.join("unsquashfs"),
        b"#!/bin/sh\nexec /usr/bin/unsquashfs -mem 16M -processors 1 \"$@\"\n",
        0o700,
    )?;
    let extraction = Command::new("rauc")
        .args([
            "extract",
            &filename.to_string_lossy(),
            &unpack.to_string_lossy(),
        ])
        .env(
            "PATH",
            format!("{}:/usr/sbin:/usr/bin:/sbin:/bin", tools.display()),
        )
        .output()
        .map_err(|e| e.to_string())?;
    if !extraction.status.success() {
        return Err(format!(
            "extract verified update bundle: {}",
            String::from_utf8_lossy(&extraction.stderr).trim()
        ));
    }
    remove_tree(&tools)?;
    let manifest = text(unpack.join("manifest.raucm"))?;
    let mut section = "";
    let mut image = None;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        }
        if section == "[image.rootfs]" {
            if let Some(value) = line.strip_prefix("filename=") {
                image = Some(value.to_owned());
            }
        }
    }
    let image = image.ok_or("update bundle has no SquashFS rootfs image")?;
    if Path::new(&image).components().count() != 1 || !paths::safe_relative(&image) {
        return Err("invalid rootfs image path".into());
    }
    let image = unpack.join(image);
    let listing = output(
        "unsquashfs",
        &[
            "-mem",
            "16M",
            "-processors",
            "1",
            "-ll",
            &image.to_string_lossy(),
            "usr/lib/modules",
        ],
    )?;
    let (releases, mut target_modules) = module_names(&listing);
    if releases.len() != 1 {
        return Err("target rootfs must identify exactly one kernel release".into());
    }
    let target_kernel = releases.into_iter().next().unwrap();
    if let Ok(builtins) = squash(
        &image,
        &format!("usr/lib/modules/{target_kernel}/modules.builtin"),
    ) {
        target_modules.extend(module_names(&builtins).1);
    }
    let current_kernel = kernel()?;
    let current_kernel_magic = kernel_identity::current_magic()?;
    let target_identity = kernel_identity::decode(
        &squash(&image, kernel_identity::METADATA.trim_start_matches('/'))
            .map_err(|_| "target image is missing kernel magic metadata; use an image with kernel identity support")?,
        &target_kernel,
    )?;
    let target_kernel_magic = target_identity.magic;
    let changed = kernel_identity::changed(
        &current_kernel,
        &current_kernel_magic,
        &target_kernel,
        &target_kernel_magic,
    );
    let target_build = String::from_utf8(squash(&image, "usr/share/onekvm/build-id")?)
        .map_err(|e| e.to_string())?
        .trim()
        .to_owned();
    let machine = squash(&image, "usr/share/onekvm/machine")?;
    if String::from_utf8_lossy(&machine).trim() != text("/usr/share/onekvm/machine")?
        || target_identity
            .machine
            .strip_prefix("onekvm-")
            .unwrap_or(&target_identity.machine)
            != String::from_utf8_lossy(&machine).trim()
    {
        return Err("target rootfs is for a different machine".into());
    }
    if changed
        && squash(&image, "usr/share/onekvm/plugin-update-version")
            .ok()
            .as_deref()
            != Some(b"1\n")
    {
        return Err("target system does not support installing plugin packages before startup; use a bundle with plugin-update support".into());
    }
    let source_slot = slot()?;
    let plugins = if changed { inventory()? } else { Vec::new() };
    let mut baseline = database(&squash(&image, "usr/share/onekvm/opkg-baseline/status")?)?;
    if changed {
        for (name, package) in &mut baseline {
            let Some(id) = name
                .strip_prefix("onekvm-extension-")
                .filter(|id| paths::valid_id(id))
            else {
                continue;
            };
            // opkg's status database drops custom fields; read the original
            // in-image control record for the extension identity.
            let fields = control::parse_control(&squash(
                &image,
                &format!("usr/share/onekvm/opkg-baseline/info/{name}.control"),
            )?)?;
            package.extension_version = fields
                .get("X-OneKVM-Extension-Version")
                .cloned()
                .unwrap_or_default();
            if !paths::valid_version(&package.extension_version) {
                continue;
            }
            let data = squash(
                &image,
                &format!(
                    "data/onekvm/apps/{id}/{}/manifest.json",
                    package.extension_version
                ),
            )?;
            package.extension = Some(manifest::decode_manifest(&data)?);
        }
    }
    let plan = Plan {
        id: text("/proc/sys/kernel/random/uuid")?,
        phase: "prepared".into(),
        bundle_sha256: hash(&filename)?,
        bundle: filename.to_string_lossy().into_owned(),
        current_kernel,
        target_kernel,
        current_kernel_magic,
        target_kernel_magic,
        source_build: build()?,
        target_build,
        target_slot: if source_slot == "A" { "B" } else { "A" }.into(),
        source_slot,
        baseline,
        retained: database(&fs::read("/var/lib/opkg/status").map_err(|e| e.to_string())?)?,
        packages: BTreeMap::new(),
        plugins,
        target_modules,
        error: String::new(),
    };
    save(&plan)?;
    // Keep only the authenticated metadata; the staged .fwup is used by RAUC.
    remove_tree(unpack)?;
    Ok(view(&plan))
}

fn add(id: &str, stdin: &mut dyn Read) -> Result<Value, String> {
    let mut plan = load()?;
    if plan.id != id || plan.phase != "prepared" {
        return Err("update preparation changed or is no longer accepting packages".into());
    }
    let uploads = root().join("uploads");
    fs::create_dir_all(&uploads).map_err(|e| e.to_string())?;
    let upload = uploads.join("incoming");
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&upload)
            .map_err(|e| e.to_string())?;
        let bytes = std::io::copy(&mut stdin.take(paths::MAX_LOCAL_IPK + 1), &mut f)
            .map_err(|e| e.to_string())?;
        if bytes == 0 || bytes > paths::MAX_LOCAL_IPK {
            return Err("plugin package must be between 1 byte and 32 MiB".into());
        }
        f.sync_all().map_err(|e| e.to_string())?;
        let architectures: BTreeSet<String> =
            String::from_utf8_lossy(&sys::opkg_output(&["print-architecture"])?)
                .lines()
                .filter_map(|s| s.split_whitespace().nth(1).map(str::to_owned))
                .collect();
        let extracted = uploads.join("extracted");
        if extracted.exists() {
            remove_tree(&extracted)?;
        }
        let bundle = ipks::is_ipks(upload.to_str().unwrap())?;
        let files = if bundle {
            ipks::inspect_ipks(upload.to_str().unwrap(), &extracted, &architectures)?.paths
        } else {
            vec![upload.clone()]
        };
        let mut additions = BTreeMap::new();
        for file in files {
            let members = ipk::read_ar(file.to_str().unwrap())?;
            let control_bytes = members
                .get("control.tar.gz")
                .ok_or("package has no control")?;
            let fields =
                ipk::inspect_control(control_bytes).or_else(|_| module_control(control_bytes))?;
            let name = fields.get("Package").ok_or("package has no name")?;
            if name.len() > 128
                || !name.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'+')
                })
            {
                return Err("invalid supplemental package name".into());
            }
            if !architectures.contains(
                fields
                    .get("Architecture")
                    .ok_or("package has no architecture")?,
            ) {
                return Err("package architecture does not match this device".into());
            }
            let data = members
                .get("data.tar.gz")
                .ok_or("package has no gzip payload")?;
            let mut value = package(&fields);
            if is_kernel_package(name) {
                kernel_identity::check_package(
                    &fields,
                    &plan.target_kernel,
                    &plan.target_kernel_magic,
                )?;
                let release = ipks::inspect_kernel_dependency_data(name, data)?;
                if release != plan.target_kernel {
                    return Err(format!(
                        "{name} targets kernel {release}, expected {}",
                        plan.target_kernel
                    ));
                }
                if name == "onekvm-kernel-modules-base" {
                    if !plan
                        .baseline
                        .get(name)
                        .is_some_and(|p| p.version == value.version)
                    {
                        return Err("bundle kernel metadata does not match the target image".into());
                    }
                    continue; // Never replace the target image's kernel ABI/build metadata.
                }
                let decoder = flate2::read::GzDecoder::new(data.as_slice());
                for entry in tar::Archive::new(decoder)
                    .entries()
                    .map_err(|e| e.to_string())?
                {
                    let mut entry = entry.map_err(|e| e.to_string())?;
                    if let Some(module) = entry
                        .path()
                        .map_err(|e| e.to_string())?
                        .file_name()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.strip_suffix(".ko"))
                    {
                        value.modules.insert(module.replace('-', "_"));
                        if entry.size() > 32 << 20 {
                            return Err("kernel module exceeds 32 MiB".into());
                        }
                        let mut module_data = Vec::new();
                        entry
                            .read_to_end(&mut module_data)
                            .map_err(|e| e.to_string())?;
                        verify_module_release(&module_data, &plan.target_kernel)?;
                    }
                }
            } else if paths::valid_package_name(name) {
                // Only optional module packages may carry the narrowly allowed depmod script.
                ipk::inspect_control(control_bytes)?;
                match ipk::inspect_data(data, name.trim_start_matches("onekvm-extension-")) {
                    Ok(m) => {
                        let m = if bundle {
                            m
                        } else {
                            ipk::inspect_ipk(file.to_str().unwrap(), name, &architectures, None)?
                                .manifest
                        };
                        if !plan.plugins.iter().any(|p| p.id == m.id) {
                            return Err(
                                "supplemental packages must belong to an installed plugin".into()
                            );
                        }
                        crate::host::validate_device_variant(&m)?;
                        value.extension = Some(m);
                    }
                    Err(_)
                        if bundle
                            && plan.plugins.iter().any(|p| {
                                name.starts_with(&format!("onekvm-extension-{}-", p.id))
                            }) =>
                    {
                        ()
                    }
                    Err(e) => return Err(e),
                }
            } else {
                return Err(
                    "provide a plugin IPKS bundle or a matching optional kernel-module IPK".into(),
                );
            }
            value.sha256 = hash(&file)?;
            value.file = format!("packages/{}-{}.ipk", name, value.sha256);
            let destination = root().join(&value.file);
            fs::create_dir_all(destination.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::copy(file, destination).map_err(|e| e.to_string())?;
            additions.insert(name.clone(), value);
        }
        if plan.packages.len() + additions.len() > MAX_PACKAGES {
            return Err("too many supplemental packages".into());
        }
        plan.packages.extend(additions);
        save(&plan)?;
        Ok(view(&plan))
    })();
    let _ = remove_tree(uploads);
    result
}

fn arm(id: &str) -> Result<Value, String> {
    let mut plan = load()?;
    if plan.id != id || plan.phase != "prepared" {
        return Err("update preparation changed".into());
    }
    if !kernel_identity::valid_magic(&plan.target_kernel_magic) {
        return Err("target kernel magic is missing; prepare an image with kernel metadata".into());
    }
    if kernel()? != plan.current_kernel
        || kernel_identity::current_magic()? != plan.current_kernel_magic
        || build()? != plan.source_build
        || slot()? != plan.source_slot
    {
        return Err("the running system changed; prepare the update again".into());
    }
    plan.plugins = inventory()?;
    plan.retained = database(&fs::read("/var/lib/opkg/status").map_err(|e| e.to_string())?)?;
    if !missing(&plan).is_empty() {
        save(&plan)?;
        return Err("required plugin packages are missing".into());
    }
    if hash(Path::new(&plan.bundle))? != plan.bundle_sha256 {
        return Err("staged update bundle changed".into());
    }
    // Pin the bundle separately from reusable HTTP/browser upload filenames.
    let pinned = root().join("update.fwup");
    if Path::new(&plan.bundle) != pinned {
        fs::copy(&plan.bundle, &pinned).map_err(|e| e.to_string())?;
        fs::File::open(&pinned)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        plan.bundle = pinned.to_string_lossy().into_owned();
    }
    // Quiesce plugin databases while taking the rollback snapshot. Source
    // settings and enabled flags are restored even if copying fails.
    let original: BTreeMap<_, _> = plan
        .plugins
        .iter()
        .map(|p| state::read_state(&p.id).map(|s| (p.id.clone(), s)))
        .collect::<Result<_, _>>()?;
    let snapshot_result = (|| {
        for (id, state) in &original {
            if state.enabled {
                crate::lifecycle::disable_unlocked(id)?;
            }
        }
        let snapshot = root().join("source");
        if snapshot.exists() {
            remove_tree(&snapshot)?;
        }
        copy_tree(Path::new(paths::APPS_ROOT), &snapshot.join("apps"))?;
        copy_tree(Path::new(paths::CONFIG_ROOT), &snapshot.join("config"))?;
        copy_tree(Path::new("/var/lib/opkg/info"), &snapshot.join("info"))?;
        // Archive extension-owned data without chowning a copied directory and
        // then attempting chmod without CAP_FOWNER in Core's CLI child. The
        // boot helper restores numeric ownership and modes with full privileges.
        let data_archive = snapshot.join("app-data.tar");
        let mut args = vec![
            "-cpf".to_owned(),
            data_archive.to_string_lossy().into_owned(),
            "-C".to_owned(),
            paths::APP_DATA_ROOT.to_owned(),
        ];
        for plugin in &plan.plugins {
            if Path::new(paths::APP_DATA_ROOT).join(&plugin.id).is_dir() {
                args.push(plugin.id.clone());
            }
        }
        if args.len() > 4 {
            sys::output("tar", &args)?;
        }
        for (id, value) in &original {
            write_json(snapshot.join("config").join(format!("{id}.json")), value)?;
        }
        Ok::<_, String>(())
    })();
    let mut restore_error = None;
    for (id, value) in &original {
        let mut restored = value.clone();
        restored.revision = state::read_state(id)
            .map(|s| s.revision)
            .unwrap_or(value.revision)
            .saturating_add(1);
        if let Err(err) = state::write_state(id, &restored).and_then(|_| {
            if value.enabled {
                crate::lifecycle::enable_unlocked(id)
            } else {
                Ok(())
            }
        }) {
            restore_error = Some(err);
        }
    }
    snapshot_result?;
    if let Some(err) = restore_error {
        return Err(format!("restore source plugin after snapshot: {err}"));
    }
    plan.phase = "armed".into();
    save(&plan)?;
    Ok(json!({"id": plan.id, "bundle": plan.bundle}))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn plan() -> Plan {
        Plan {
            id: "test".into(),
            phase: "prepared".into(),
            bundle: String::new(),
            bundle_sha256: String::new(),
            current_kernel: "6.18.45-onekvm-preempt".into(),
            target_kernel: "6.18.46-onekvm-preempt".into(),
            current_kernel_magic: format!("sha256:{}", "a".repeat(64)),
            target_kernel_magic: format!("sha256:{}", "b".repeat(64)),
            source_build: "same-system-version".into(),
            target_build: "same-system-version".into(),
            source_slot: "A".into(),
            target_slot: "B".into(),
            baseline: BTreeMap::new(),
            retained: BTreeMap::new(),
            packages: BTreeMap::new(),
            plugins: vec![Plugin {
                id: "vpn".into(),
                name: "VPN".into(),
                version: "1".into(),
                depends: Some("onekvm-kernel-module-tun (= 2)".into()),
                kernel_modules: vec!["tun".into()],
            }],
            target_modules: BTreeSet::new(),
            error: String::new(),
        }
    }
    fn pkg(version: &str, depends: &str) -> Package {
        Package {
            version: version.into(),
            depends: depends.into(),
            ..Package::default()
        }
    }
    #[test]
    fn identical_kernel_identity_skips_plugin_requirements() {
        let mut p = plan();
        p.target_kernel = p.current_kernel.clone();
        p.target_kernel_magic = p.current_kernel_magic.clone();
        p.plugins[0].depends = None;
        assert!(missing(&p).is_empty());
        assert_eq!(view(&p)["kernel_changed"], false);
    }
    #[test]
    fn same_release_with_new_magic_or_legacy_metadata_checks_modules() {
        let mut p = plan();
        p.target_kernel = p.current_kernel.clone();
        assert_eq!(view(&p)["kernel_changed"], true);
        assert!(!missing(&p).is_empty());
        p.current_kernel_magic.clear();
        p.target_kernel_magic.clear();
        assert_eq!(view(&p)["kernel_changed"], true);
        assert!(!missing(&p).is_empty());
        p.plugins.clear();
        assert_eq!(view(&p)["ready"], false);
    }
    #[test]
    fn system_version_does_not_control_the_kernel_gate() {
        let mut p = plan();
        p.target_kernel = "6.18.45-onekvm".into();
        assert_eq!(p.source_build, p.target_build);
        assert_eq!(view(&p)["kernel_changed"], true);
        assert!(!missing(&p).is_empty());
    }
    #[test]
    fn old_kernel_packages_cannot_satisfy_target_requirements() {
        let mut p = plan();
        p.retained
            .insert("onekvm-kernel-module-tun".into(), pkg("2", ""));
        let m = missing(&p);
        assert!(m
            .iter()
            .any(|m| m["package"] == "onekvm-kernel-module-tun (= 2)"));
        assert_eq!(view(&p)["ready"], false);
    }
    #[test]
    fn target_builtin_module_needs_no_upload() {
        let mut p = plan();
        p.baseline
            .insert("onekvm-kernel-module-tun".into(), pkg("2", ""));
        p.target_modules.insert("tun".into());
        assert!(missing(&p).is_empty());
        assert_eq!(view(&p)["ready"], true);
    }
    #[test]
    fn supplied_modules_must_have_the_complete_dependency_closure() {
        let mut p = plan();
        p.packages.insert(
            "onekvm-kernel-module-tun".into(),
            Package {
                modules: BTreeSet::from(["tun".into()]),
                ..pkg("2", "onekvm-kernel-modules-base (= 2)")
            },
        );
        assert!(missing(&p)
            .iter()
            .any(|m| m["package"] == "onekvm-kernel-modules-base (= 2)"));
        p.baseline
            .insert("onekvm-kernel-modules-base".into(), pkg("1", ""));
        assert!(!missing(&p).is_empty());
        p.baseline
            .insert("onekvm-kernel-modules-base".into(), pkg("2", ""));
        assert!(missing(&p).is_empty());
    }
    #[test]
    fn module_tree_uses_full_release_and_normalizes_module_names() {
        let (kernels, modules) = module_names(b"drwxr-xr-x root/root 3 2026-01-01 squashfs-root/usr/lib/modules/6.18.45-onekvm-preempt/updates\n-rw-r--r-- root/root 3 2026-01-01 squashfs-root/usr/lib/modules/6.18.45-onekvm-preempt/updates/ip-tunnel.ko\n");
        assert_eq!(kernels, BTreeSet::from(["6.18.45-onekvm-preempt".into()]));
        assert!(modules.contains("ip_tunnel"));
    }
    #[test]
    fn renaming_an_old_module_directory_cannot_bypass_vermagic() {
        assert!(verify_module_release(
            b"\x7fELF\0vermagic=6.18.45-onekvm-preempt SMP\0",
            "6.18.46-onekvm-preempt"
        )
        .is_err());
        assert!(verify_module_release(
            b"\x7fELF\0vermagic=6.18.46-onekvm-preempt SMP\0",
            "6.18.46-onekvm-preempt"
        )
        .is_ok());
        assert!(verify_module_release(b"\x7fELF", "6.18.46-onekvm-preempt").is_err());
    }
    #[test]
    fn target_baseline_wins_over_old_userspace_versions() {
        let mut p = plan();
        p.plugins[0].kernel_modules.clear();
        p.plugins[0].depends = Some("libssl3 (>= 3.5)".into());
        p.retained.insert("libssl3".into(), pkg("3.6", ""));
        p.baseline.insert("libssl3".into(), pkg("3.4", ""));
        assert!(!missing(&p).is_empty());
    }
    #[test]
    fn unknown_plugin_metadata_requires_a_replacement_package() {
        let mut p = plan();
        p.plugins[0].depends = None;
        assert!(missing(&p)
            .iter()
            .any(|m| m["package"] == "onekvm-extension-vpn"));
        p.packages.insert(
            "onekvm-extension-vpn".into(),
            Package {
                extension: Some(manifest::Manifest {
                    kernel_modules: Vec::new(),
                    ..manifest::Manifest::default()
                }),
                ..pkg("2", "")
            },
        );
        assert!(missing(&p).is_empty());
    }
}

fn bind(from: &Path, to: &Path) -> Result<(), String> {
    if output("mountpoint", &["-q", &to.to_string_lossy()]).is_ok() {
        return Ok(());
    }
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    output(
        "mount",
        &["--bind", &from.to_string_lossy(), &to.to_string_lossy()],
    )?;
    Ok(())
}
fn mount_slot(value: &Slot) -> Result<(), String> {
    bind(
        &Path::new(&value.directory).join("apps"),
        Path::new(paths::APPS_ROOT),
    )?;
    bind(
        &Path::new(&value.directory).join("config"),
        Path::new(paths::CONFIG_ROOT),
    )?;
    for id in &value.plugins {
        if !paths::valid_id(id) {
            return Err("invalid plugin slot mapping".into());
        }
        let source = Path::new(&value.directory).join("app-data").join(id);
        fs::create_dir_all(&source).map_err(|e| e.to_string())?;
        bind(&source, &Path::new(paths::APP_DATA_ROOT).join(id))?;
    }
    Ok(())
}
fn boot() -> Result<Value, String> {
    let current_slot = match slot() {
        Ok(s) => s,
        Err(_) => return Ok(json!({"phase":"idle"})),
    };
    let current_kernel = kernel()?;
    let current_build = build()?;
    let current_kernel_magic = kernel_identity::current_magic()?;
    let mapping = Path::new(SLOTS).join(&current_slot).join("current.json");
    if let Ok(value) = read_json::<Slot>(&mapping) {
        if value.build == current_build
            && value.kernel == current_kernel
            && value.kernel_magic == current_kernel_magic
        {
            mount_slot(&value)?;
            // Extensions can load modules before onekvm-ready.target on every
            // boot, so restore this bind before returning a completed plan too.
            output("/usr/libexec/onekvm/prepare-persistence-modules", &[])?;
        }
    }
    let mut plan = match load() {
        Ok(p) => p,
        Err(_) if !root().join("plan.json").exists() => return Ok(json!({"phase":"idle"})),
        Err(e) => return Err(e),
    };
    // Older upload clients leave same-kernel updates in `prepared`: no package
    // approval or plugin snapshot is needed, but the new slot's opkg baseline
    // and optional-module bind still need the installed plugins' dependencies.
    if plan.phase == "prepared"
        && !kernel_changed(&plan)
        && plan.target_slot == current_slot
        && plan.target_build == current_build
        && plan.target_kernel == current_kernel
        && plan.target_kernel_magic == current_kernel_magic
    {
        match retained::restore(&plan) {
            Ok(()) => plan.phase = "complete".into(),
            Err(error) => {
                // One damaged optional plugin must not prevent every other
                // extension from starting on an otherwise compatible kernel.
                eprintln!("same-kernel plugin dependency restore failed: {error}");
                plan.phase = "failed".into();
                plan.error = error;
            }
        }
        save(&plan)?;
        return Ok(view(&plan));
    }
    if matches!(plan.phase.as_str(), "armed" | "installing")
        && !kernel_identity::valid_magic(&plan.target_kernel_magic)
    {
        return Err(
            "pending update has no target kernel magic; prepare it again before installing modules"
                .into(),
        );
    }
    if plan.target_slot == current_slot
        && plan.target_build == current_build
        && matches!(plan.phase.as_str(), "armed" | "installing")
        && (plan.target_kernel != current_kernel
            || plan.target_kernel_magic != current_kernel_magic)
    {
        return Err(format!(
            "booted kernel {current_kernel} or its magic does not match prepared target {}",
            plan.target_kernel
        ));
    }
    if plan.target_slot != current_slot
        || plan.target_build != current_build
        || plan.target_kernel != current_kernel
        || plan.target_kernel_magic != current_kernel_magic
        || !matches!(plan.phase.as_str(), "armed" | "installing")
    {
        return Ok(view(&plan));
    }
    if hash(Path::new(&plan.bundle))? != plan.bundle_sha256 {
        return Err("staged firmware changed before boot".into());
    }
    for p in plan.packages.values() {
        if hash(&root().join(&p.file))? != p.sha256 {
            return Err("staged plugin package changed before boot".into());
        }
    }
    let directory = Path::new(SLOTS).join(&current_slot).join(&plan.id);
    if plan.phase == "armed" {
        // A kernel change can intentionally retain the same system build ID.
        // Rebase the target slot's package database regardless of that ID so
        // old kernel packages can never be treated as already installed.
        let opkg = Path::new(paths::OPKG_SLOTS)
            .join(&current_slot)
            .join("opkg");
        if fs::canonicalize("/var/lib/opkg").map_err(|e| e.to_string())?
            != fs::canonicalize(&opkg).map_err(|e| e.to_string())?
        {
            return Err("opkg database does not belong to the booted slot".into());
        }
        fs::remove_dir_all(&opkg).map_err(|e| e.to_string())?;
        copy_tree(Path::new("/usr/share/onekvm/opkg-baseline"), &opkg)?;
        sys::atomic_write(
            &opkg.join(".baseline-build-id"),
            format!("{current_build}\n").as_bytes(),
            0o644,
        )?;
        sys::atomic_write(
            &opkg.join(".baseline-kernel-magic"),
            format!("{current_kernel_magic}\n").as_bytes(),
            0o644,
        )?;
        copy_tree(&root().join("source/apps"), &directory.join("apps"))?;
        copy_tree(&root().join("source/config"), &directory.join("config"))?;
        let data_archive = root().join("source/app-data.tar");
        if data_archive.is_file() {
            let data = directory.join("app-data");
            fs::create_dir_all(&data).map_err(|e| e.to_string())?;
            output(
                "tar",
                &[
                    "-xpf",
                    &data_archive.to_string_lossy(),
                    "-C",
                    &data.to_string_lossy(),
                ],
            )?;
        }
        // Overlay the target image's built-in plugins on the copied user plugins.
        if let Some(immutable) = ["/mnt/.rofs/data/onekvm/apps", "/rofs/data/onekvm/apps"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_dir())
        {
            copy_tree(immutable, &directory.join("apps"))?;
        }
        write_json(
            &mapping,
            &Slot {
                build: current_build,
                kernel: current_kernel,
                kernel_magic: current_kernel_magic,
                directory: directory.to_string_lossy().into_owned(),
                plugins: plan.plugins.iter().map(|p| p.id.clone()).collect(),
            },
        )?;
        plan.phase = "installing".into();
        save(&plan)?;
    }
    mount_slot(&read_json(&mapping)?)?;
    // Bind the target slot's optional module tree before opkg writes any .ko.
    output("/usr/libexec/onekvm/prepare-persistence-modules", &[])?;
    // No software source is configured for this transaction: all dependencies must
    // come from the target image, retained userspace, or the uploaded local files.
    let config = root().join("offline.conf");
    let mut conf = String::from("dest root /\noption info_dir /var/lib/opkg/info\noption status_file /var/lib/opkg/status\noption lists_dir /data/rauc/plugin-update/empty-lists\noption cache_dir /data/rauc/plugin-update/cache\n");
    for line in String::from_utf8_lossy(&sys::opkg_output(&["print-architecture"])?).lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() == 3 {
            conf.push_str(&format!("arch {} {}\n", fields[1], fields[2]));
        }
    }
    sys::atomic_write(&config, conf.as_bytes(), 0o600)?;
    fs::create_dir_all(root().join("empty-lists")).map_err(|e| e.to_string())?;
    // Userspace runtimes installed outside the image survive on the root overlay.
    // Re-register only reachable, compatible records; never import old kernel state.
    let mut needed = BTreeSet::new();
    let mut queue: Vec<String> = plan
        .plugins
        .iter()
        .filter_map(|p| p.depends.clone())
        .collect();
    queue.extend(plan.packages.values().map(|p| p.depends.clone()));
    while let Some(depends) = queue.pop() {
        for expression in depends.split(',') {
            for alternative in expression.split('|') {
                let Ok(parsed) = ipk::parse_package_dependencies(alternative.trim()) else {
                    continue;
                };
                let Some(dep) = parsed.first() else {
                    continue;
                };
                if is_kernel_package(&dep.name)
                    || plan.baseline.contains_key(&dep.name)
                    || plan.packages.contains_key(&dep.name)
                {
                    continue;
                }
                if let Some(p) = plan.retained.get(&dep.name) {
                    if version_matches(&p.version, &dep.compare, &dep.version)
                        && needed.insert(dep.name.clone())
                    {
                        queue.push(p.depends.clone());
                    }
                }
            }
        }
    }
    let mut status = fs::read_to_string("/var/lib/opkg/status").map_err(|e| e.to_string())?;
    let existing = database(status.as_bytes())?;
    for name in needed {
        if existing.contains_key(&name) {
            continue;
        }
        let source = root().join("source/info").join(format!("{name}.control"));
        let record = fs::read_to_string(&source)
            .map_err(|e| format!("retained runtime {name} metadata: {e}"))?;
        let fields = control::parse_control(record.as_bytes())?;
        if fields.get("Package") != Some(&name)
            || fields.get("Version") != Some(&plan.retained[&name].version)
        {
            return Err("retained runtime metadata changed".into());
        }
        status.push_str(&format!(
            "\n\n{}\nStatus: install ok installed\n\n",
            record.trim()
        ));
        for entry in fs::read_dir(root().join("source/info")).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{name}."))
            {
                fs::copy(
                    entry.path(),
                    Path::new("/var/lib/opkg/info").join(entry.file_name()),
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }
    sys::atomic_write(Path::new("/var/lib/opkg/status"), status.as_bytes(), 0o644)?;
    if !plan.packages.is_empty() {
        let mut args = vec![
            "--conf".into(),
            config.to_string_lossy().into_owned(),
            "--force-reinstall".into(),
            "install".into(),
        ];
        args.extend(
            plan.packages
                .values()
                .map(|p| root().join(&p.file).to_string_lossy().into_owned()),
        );
        sys::output(paths::OPKG, &args)?;
    }
    for plugin in &plan.plugins {
        let name = format!("onekvm-extension-{}", plugin.id);
        let version = plan
            .packages
            .get(&name)
            .and_then(|p| p.extension.as_ref())
            .map(|m| m.version.clone())
            .or_else(|| {
                plan.baseline.get(&name).and_then(|_| {
                    // The in-image package control records the extension version.
                    text(format!(
                        "/usr/share/onekvm/opkg-baseline/info/{name}.control"
                    ))
                    .ok()
                    .and_then(|s| control::parse_control(s.as_bytes()).ok())
                    .and_then(|m| m.get("X-OneKVM-Extension-Version").cloned())
                })
            })
            .unwrap_or_else(|| plugin.version.clone());
        let installed = manifest::load_installed(&plugin.id, &version)?;
        let mut state = state::read_state(&plugin.id)?;
        state.active_version = version;
        state.revision = state.revision.saturating_add(1);
        state::normalize_environment(&installed.manifest, &mut state);
        state::write_state(&plugin.id, &state)?;
    }
    output(paths::DEPMOD, &["-a"])?;
    for plugin in &plan.plugins {
        let state = state::read_state(&plugin.id)?;
        if state.enabled {
            let installed = manifest::load_installed(&plugin.id, &state.active_version)?;
            if installed.manifest.is_environment() {
                continue;
            }
            for module in &installed.manifest.kernel_modules {
                output(paths::MODPROBE, &[module])?;
            }
        }
    }
    plan.phase = "complete".into();
    save(&plan)?;
    // The source slot still owns its original files. Only the target slot's
    // independent directories are needed once installation has completed.
    let _ = remove_tree(root().join("source"));
    let _ = remove_tree(root().join("packages"));
    let _ = fs::remove_file(&plan.bundle);
    Ok(view(&plan))
}

pub fn command(args: &[String], stdin: &mut dyn Read) -> Result<Value, String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["update-prepare", file] => prepare(file),
        ["update-status"] => match load() {
            Ok(p) => Ok(view(&p)),
            Err(_) if !root().join("plan.json").exists() => Ok(json!({"phase":"idle"})),
            Err(e) => Err(e),
        },
        ["update-add", id] => add(id, stdin),
        ["update-arm", id] => arm(id),
        ["update-disarm", id] => {
            let mut p = load()?;
            if p.id != *id || p.phase != "armed" {
                return Err("update preparation changed".into());
            }
            p.phase = "prepared".into();
            save(&p)?;
            Ok(view(&p))
        }
        ["update-cancel", id] => {
            let p = load()?;
            if p.id != *id || p.phase != "prepared" {
                return Err("cannot cancel an update already being installed".into());
            }
            remove_tree(root())?;
            Ok(json!({"phase":"idle"}))
        }
        ["update-boot"] => match boot() {
            Ok(value) => Ok(value),
            Err(err) => {
                if let Ok(mut p) = load() {
                    if slot().ok().as_deref() == Some(&p.target_slot)
                        && build().ok().as_deref() == Some(&p.target_build)
                    {
                        p.phase = "failed".into();
                        p.error = err.clone();
                        let _ = save(&p);
                    }
                }
                Err(err)
            }
        },
        _ => Err("invalid update helper command".into()),
    }
}
