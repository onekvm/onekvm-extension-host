//! Carry optional plugin packages across slots only for an identical kernel.
//! The target image's packages always win; source package scripts are not run.
use super::*;
use std::os::unix::fs::MetadataExt;

fn closure(
    roots: &[String],
    target: &BTreeMap<String, Package>,
    source: &BTreeMap<String, Package>,
) -> Result<BTreeSet<String>, String> {
    let mut needed = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut queue = roots.to_vec();
    while let Some(name) = queue.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let package = target
            .get(&name)
            .or_else(|| source.get(&name))
            .ok_or_else(|| format!("retained package {name} is missing"))?;
        if target.contains_key(&name) {
            // The target image owns this package and its dependency closure.
            continue;
        }
        needed.insert(name.clone());
        for expression in package
            .depends
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let mut selected = None;
            for alternative in expression.split('|') {
                let parsed = ipk::parse_package_dependencies(alternative.trim())?;
                let Some(dep) = parsed.first() else { continue };
                let candidate = target
                    .get_key_value(&dep.name)
                    .or_else(|| {
                        target
                            .iter()
                            .find(|(_, p)| p.provides.split(',').any(|v| v.trim() == dep.name))
                    })
                    .or_else(|| source.get_key_value(&dep.name))
                    .or_else(|| {
                        source
                            .iter()
                            .find(|(_, p)| p.provides.split(',').any(|v| v.trim() == dep.name))
                    });
                if let Some((name, _)) = candidate
                    .filter(|(_, p)| version_matches(&p.version, &dep.compare, &dep.version))
                {
                    selected = Some(name.clone());
                    break;
                }
            }
            queue.push(selected.ok_or_else(|| format!("retained package {name} requires {expression}; reinstall a compatible plugin package"))?);
        }
    }
    Ok(needed)
}

fn module_source(
    path: &Path,
    source_modules: &Path,
    release: &str,
) -> Result<Option<PathBuf>, String> {
    use std::path::Component;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("invalid retained package file path".into());
    }
    let modules = PathBuf::from(format!("/usr/lib/modules/{release}/updates"));
    if let Ok(relative) = path.strip_prefix(modules) {
        return Ok(Some(source_modules.join(relative)));
    }
    if path.starts_with("/usr/lib/modules") || path.starts_with("/lib/modules") {
        return Err("retained module is outside the optional updates tree".into());
    }
    Ok(None)
}

fn restore_files(
    plan: &Plan,
    needed: &BTreeSet<String>,
    source_info: &Path,
    target_db: &Path,
    source_modules: &Path,
) -> Result<(), String> {
    let mut status = fs::read_to_string(target_db.join("status")).map_err(|e| e.to_string())?;
    let existing = database(status.as_bytes())?;
    let mut records = Vec::new();
    let mut modules = Vec::new();
    for name in needed {
        let record = text(source_info.join(format!("{name}.control")))?;
        let fields = control::parse_control(record.as_bytes())?;
        if fields.get("Package") != Some(name)
            || fields.get("Version") != Some(&plan.retained[name].version)
        {
            return Err(format!("retained package {name} metadata changed"));
        }
        kernel_identity::check_package(&fields, &plan.target_kernel, &plan.target_kernel_magic)?;
        let list = text(source_info.join(format!("{name}.list")))?;
        for line in list.lines() {
            let mut parts = line.split_whitespace();
            let Some(filename) = parts.next() else {
                continue;
            };
            if parts
                .next()
                .and_then(|mode| u32::from_str_radix(mode, 8).ok())
                .is_some_and(|mode| mode & 0o170000 == 0o040000)
            {
                continue;
            }
            let destination = PathBuf::from(filename);
            let source = module_source(&destination, source_modules, &plan.target_kernel)?;
            let source = source.as_deref().unwrap_or(&destination);
            let metadata = fs::symlink_metadata(source)
                .map_err(|e| format!("retained package {name} file {}: {e}", source.display()))?;
            if metadata.is_dir() {
                continue;
            }
            if source != destination {
                if !metadata.is_file() || metadata.len() > 32 << 20 {
                    return Err("invalid retained optional module".into());
                }
                let bytes = fs::read(source).map_err(|e| e.to_string())?;
                verify_module_release(&bytes, &plan.target_kernel)?;
                modules.push((destination, bytes, metadata.mode() & 0o777));
            }
        }
        records.push((name, record));
    }
    // Validate every record and file before registering any package. Publishing
    // status last makes an interrupted copy safe to retry on the next boot.
    for (destination, bytes, mode) in modules {
        sys::atomic_write(&destination, &bytes, mode)?;
    }
    for (name, record) in records {
        for entry in fs::read_dir(source_info).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{name}."))
            {
                fs::copy(entry.path(), target_db.join("info").join(entry.file_name()))
                    .map_err(|e| e.to_string())?;
            }
        }
        if !existing.contains_key(name) {
            status.push_str(&format!("\n\n{record}\nStatus: install ok installed\n\n"));
        }
    }
    sys::atomic_write(&target_db.join("status"), status.as_bytes(), 0o644)
}

pub(super) fn restore(plan: &Plan) -> Result<(), String> {
    if kernel_changed(plan)
        || !matches!(plan.source_slot.as_str(), "A" | "B")
        || plan.source_slot == plan.target_slot
    {
        return Err(
            "retained packages require distinct slots with identical kernel identity".into(),
        );
    }
    let source_db = Path::new(paths::OPKG_SLOTS)
        .join(&plan.source_slot)
        .join("opkg");
    if text(source_db.join(".baseline-build-id"))? != plan.source_build
        || text(source_db.join(".baseline-kernel-magic"))? != plan.current_kernel_magic
    {
        return Err("retained package source slot changed".into());
    }
    let target_db = Path::new(paths::OPKG_SLOTS)
        .join(&plan.target_slot)
        .join("opkg");
    if fs::canonicalize("/var/lib/opkg").map_err(|e| e.to_string())?
        != fs::canonicalize(&target_db).map_err(|e| e.to_string())?
    {
        return Err("opkg database does not belong to the booted slot".into());
    }
    let target = database(&fs::read(target_db.join("status")).map_err(|e| e.to_string())?)?;
    let mut roots = Vec::new();
    for plugin in inventory()? {
        let name = format!("onekvm-extension-{}", plugin.id);
        // Unmanaged plugin directories have no package records to carry.
        if !plan.retained.contains_key(&name) || target.contains_key(&name) {
            continue;
        }
        let fields = control::parse_control(
            &fs::read(source_db.join("info").join(format!("{name}.control")))
                .map_err(|e| e.to_string())?,
        )?;
        if fields.get("X-OneKVM-Extension-Version") != Some(&plugin.version) {
            return Err(format!("retained plugin {} version changed", plugin.id));
        }
        roots.push(name);
    }
    let needed = closure(&roots, &target, &plan.retained)?;
    let hash = plan
        .current_kernel_magic
        .strip_prefix("sha256:")
        .ok_or("missing kernel magic")?;
    let source_modules = Path::new("/mnt/storage/onekvm/kernel-modules")
        .join(&plan.source_slot)
        .join(&plan.current_kernel)
        .join(hash)
        .join("updates");
    restore_files(
        plan,
        &needed,
        &source_db.join("info"),
        &target_db,
        &source_modules,
    )?;
    output(paths::DEPMOD, &["-a", &plan.target_kernel])?;
    eprintln!(
        "retained {} plugin packages for identical kernel {}",
        needed.len(),
        plan.target_kernel_magic
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pkg(version: &str, depends: &str) -> Package {
        Package {
            version: version.into(),
            depends: depends.into(),
            ..Package::default()
        }
    }
    #[test]
    fn carries_plugin_runtime_and_optional_module_transitively() {
        let source = BTreeMap::from([
            (
                "onekvm-extension-samba".into(),
                pkg("1", "samba-runtime, cifs"),
            ),
            ("samba-runtime".into(), pkg("1", "musl")),
            ("cifs".into(), pkg("1", "aes (= 1)")),
            ("aes".into(), pkg("1", "kernel-base (= 1)")),
            ("unrelated".into(), pkg("1", "")),
        ]);
        let target = BTreeMap::from([
            ("musl".into(), pkg("2", "")),
            ("kernel-base".into(), pkg("1", "")),
        ]);
        assert_eq!(
            closure(&["onekvm-extension-samba".into()], &target, &source).unwrap(),
            BTreeSet::from(
                ["onekvm-extension-samba", "samba-runtime", "cifs", "aes"].map(str::to_owned)
            )
        );
    }
    #[test]
    fn image_packages_win_and_incompatible_dependencies_are_rejected() {
        let source = BTreeMap::from([
            ("plugin".into(), pkg("1", "library (>= 2)")),
            ("library".into(), pkg("3", "")),
        ]);
        let target = BTreeMap::from([("library".into(), pkg("1", ""))]);
        assert!(closure(&["plugin".into()], &target, &source).is_err());
        assert!(closure(&["library".into()], &target, &source)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn alternatives_and_virtual_providers_choose_target_before_retained() {
        let source = BTreeMap::from([
            ("plugin".into(), pkg("1", "absent | virtual, plugin")),
            ("virtual".into(), pkg("1", "")),
        ]);
        let target = BTreeMap::from([(
            "provider".into(),
            Package {
                provides: "virtual".into(),
                ..pkg("1", "")
            },
        )]);
        assert_eq!(
            closure(&["plugin".into()], &target, &source).unwrap(),
            BTreeSet::from(["plugin".into()])
        );
    }
    #[test]
    fn modules_are_taken_only_from_the_matching_slot_optional_tree() {
        let root = Path::new("/saved/updates");
        assert_eq!(
            module_source(
                Path::new("/usr/lib/modules/6.18/updates/cifs/cifs.ko"),
                root,
                "6.18"
            )
            .unwrap(),
            Some(root.join("cifs/cifs.ko"))
        );
        assert!(module_source(
            Path::new("/usr/lib/modules/other/updates/cifs.ko"),
            root,
            "6.18"
        )
        .is_err());
        assert!(module_source(
            Path::new("/usr/lib/modules/6.18/updates/../cifs.ko"),
            root,
            "6.18"
        )
        .is_err());
    }

    #[test]
    fn missing_payload_does_not_register_packages_and_retry_is_idempotent() {
        let root = std::env::temp_dir().join(format!("onekvm-retained-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(target.join("info")).unwrap();
        let original =
            "Package: musl\nVersion: 2\nArchitecture: all\nStatus: install ok installed\n\n";
        fs::write(target.join("status"), original).unwrap();
        let mut plan = super::super::tests::plan();
        plan.retained.insert("optional".into(), pkg("1", "musl"));
        fs::write(
            source.join("optional.control"),
            "Package: optional\nVersion: 1\nArchitecture: all\nDepends: musl\n",
        )
        .unwrap();
        let payload = root.join("runtime");
        fs::write(
            source.join("optional.list"),
            format!("{}\t0100755\n", payload.display()),
        )
        .unwrap();
        let needed = BTreeSet::from(["optional".into()]);
        assert!(restore_files(&plan, &needed, &source, &target, Path::new("/unused")).is_err());
        assert_eq!(fs::read_to_string(target.join("status")).unwrap(), original);
        fs::write(payload, "runtime").unwrap();
        restore_files(&plan, &needed, &source, &target, Path::new("/unused")).unwrap();
        let once = fs::read(target.join("status")).unwrap();
        restore_files(&plan, &needed, &source, &target, Path::new("/unused")).unwrap();
        assert_eq!(fs::read(target.join("status")).unwrap(), once);
        assert_eq!(database(&once).unwrap()["optional"].version, "1");
        assert!(target.join("info/optional.control").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
