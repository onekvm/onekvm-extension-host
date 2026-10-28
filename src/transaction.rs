//! Recoverable transactions for one extension's payload, data and package state.
//! Shared platform dependencies retain their own opkg state; they are not
//! silently downgraded when an extension is rolled back.
use crate::{
    paths,
    state::{self, PluginState},
    sys,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const ROOT: &str = "/data/pkg-state/extensions/transactions";
const STATUS: &str = "/var/lib/opkg/status";
const INFO: &str = "/var/lib/opkg/info";

#[derive(Clone)]
struct Layout {
    root: PathBuf,
    apps: PathBuf,
    data: PathBuf,
    info: PathBuf,
    status: PathBuf,
}
impl Default for Layout {
    fn default() -> Self {
        Self {
            root: ROOT.into(),
            apps: paths::APPS_ROOT.into(),
            data: paths::APP_DATA_ROOT.into(),
            info: INFO.into(),
            status: STATUS.into(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(skip)]
    layout: Layout,
    id: String,
    pub state: PluginState,
    state_exists: bool,
    prepared: bool,
    packages: BTreeSet<String>,
    status: String,
}

fn package_name(paragraph: &str) -> &str {
    paragraph
        .lines()
        .find_map(|line| line.strip_prefix("Package: "))
        .unwrap_or_default()
        .trim()
}

fn owned_packages(status: &str, id: &str, layout: &Layout) -> BTreeSet<String> {
    let root = format!("onekvm-extension-{id}");
    let owned = layout.apps.join(id);
    status
        .split("\n\n")
        .filter_map(|paragraph| {
            let name = package_name(paragraph);
            if name == root {
                return Some(name.to_string());
            }
            if !name.starts_with(&format!("{root}-"))
                || paragraph.contains("X-OneKVM-Extension-Manifest:")
            {
                return None;
            }
            let files = fs::read_to_string(layout.info.join(format!("{name}.list"))).ok()?;
            let mut has_owned = false;
            for file in files.lines().filter(|line| !line.is_empty()) {
                let file = Path::new(file);
                if file.starts_with(&owned) {
                    has_owned = true;
                } else if !owned.starts_with(file) {
                    return None;
                }
            }
            has_owned.then(|| name.to_string())
        })
        .collect()
}

impl Snapshot {
    fn directory(&self) -> PathBuf {
        self.layout.root.join(&self.id)
    }
    fn payload_lock(&self) -> Result<File, String> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(self.directory().join("payload.lock"))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(file)
    }
    fn save(&self) -> Result<(), String> {
        sys::atomic_write(
            &self.directory().join("journal.json"),
            &serde_json::to_vec(self).map_err(|e| e.to_string())?,
            0o600,
        )
    }
    /// Persist original state before disabling, so interruption during quiesce
    /// can be recovered without requiring a complete payload snapshot.
    pub fn begin(id: &str) -> Result<Self, String> {
        if !paths::valid_id(id) {
            return Err("invalid transaction extension id".into());
        }
        if Path::new(ROOT).join(id).exists() {
            return Err(format!("unfinished extension transaction: {id}"));
        }
        let status = fs::read_to_string(STATUS).unwrap_or_default();
        let packages = owned_packages(&status, id, &Layout::default());
        let status = status
            .split("\n\n")
            .filter(|part| packages.contains(package_name(part)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let snapshot = Self {
            layout: Layout::default(),
            id: id.into(),
            state: state::read_state(id)?,
            state_exists: paths::state_path(id).exists(),
            prepared: false,
            packages,
            status,
        };
        if let Err(error) = snapshot.save() {
            let _ = crate::install::remove_extension_tree(&snapshot.directory());
            return Err(error);
        }
        Ok(snapshot)
    }
    /// Must be called after all extension processes have stopped.
    pub fn capture(&mut self) -> Result<(), String> {
        if !has_payload_access() {
            crate::dbus_client::transaction("CaptureTransaction", &self.id)?;
            *self = load(&self.id)?;
            return Ok(());
        }
        self.capture_local()
    }
    fn capture_local(&mut self) -> Result<(), String> {
        let _lock = self.payload_lock()?;
        let path = self.directory().join("payload.tar");
        let file = File::create(&path).map_err(|e| e.to_string())?;
        let mut archive = tar::Builder::new(file);
        archive.follow_symlinks(false);
        for (key, path) in [
            ("apps", self.layout.apps.join(&self.id)),
            ("data", self.layout.data.join(&self.id)),
        ] {
            if fs::symlink_metadata(&path).is_ok() {
                archive
                    .append_dir_all(key, path)
                    .map_err(|e| e.to_string())?;
            }
        }
        if let Ok(entries) = fs::read_dir(&self.layout.info) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if self
                    .packages
                    .iter()
                    .any(|package| name.starts_with(&format!("{package}.")))
                {
                    archive
                        .append_path_with_name(entry.path(), Path::new("info").join(name))
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        archive
            .into_inner()
            .map_err(|e| e.to_string())?
            .sync_all()
            .map_err(|e| e.to_string())?;
        self.prepared = true;
        self.save()
    }
    pub fn commit(&self) -> Result<(), String> {
        let _lock = self.payload_lock()?;
        // Mark committed before cleanup: a cleanup failure must never undo a
        // healthy upgrade on the next invocation.
        sys::atomic_write(&self.directory().join("committed"), b"1\n", 0o600)?;
        crate::contributions::changed()?;
        crate::install::remove_extension_tree(&self.directory()).map_err(|e| e.to_string())
    }
    pub fn rollback(&self, restart: bool) -> Result<(), String> {
        // A timed-out D-Bus call may still own archive I/O in the host. Wait
        // before touching data or restarting the old process; commit also
        // locks before cleanup, so a late capture cannot recreate its journal.
        drop(self.payload_lock()?);
        if let Ok(installed) = crate::manifest::load_installed(&self.id, "") {
            crate::host::stop_declared_service_units(&installed.manifest, false)?;
        }
        crate::host::stop_extension_runtime(&self.id)?;
        if self.prepared {
            self.restore_payload()?;
        }
        let current = match state::read_state(&self.id) {
            Ok(state) => state,
            Err(_) => {
                fs::remove_file(paths::state_path(&self.id)).map_err(|e| e.to_string())?;
                PluginState::default()
            }
        };
        let mut restored = self.state.clone();
        restored.revision = current.revision.max(restored.revision).saturating_add(1);
        restored.lifecycle_revision = current
            .lifecycle_revision
            .max(restored.lifecycle_revision)
            .saturating_add(1);
        state::write_state(&self.id, &restored)?;
        if !self.state_exists {
            fs::remove_file(paths::state_path(&self.id)).map_err(|e| e.to_string())?;
        }
        if restart
            && restored.enabled
            && crate::manifest::load_installed_optional(&self.id, &restored.active_version)?
                .is_some()
        {
            crate::lifecycle::enable_unlocked(&self.id)?;
        }
        crate::contributions::changed()?;
        self.commit()
    }
    fn restore_payload(&self) -> Result<(), String> {
        if !has_payload_access() {
            return crate::dbus_client::transaction("RestoreTransaction", &self.id);
        }
        self.restore_payload_local()
    }
    fn restore_payload_local(&self) -> Result<(), String> {
        let _lock = self.payload_lock()?;
        for path in [
            self.layout.apps.join(&self.id),
            self.layout.data.join(&self.id),
        ] {
            if path.exists() {
                crate::install::remove_extension_tree(&path).map_err(|e| e.to_string())?;
            }
        }
        let status = fs::read_to_string(&self.layout.status).unwrap_or_default();
        let mut packages = owned_packages(&status, &self.id, &self.layout);
        packages.extend(self.packages.iter().cloned());
        if let Ok(entries) = fs::read_dir(&self.layout.info) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if packages
                    .iter()
                    .any(|package| name.starts_with(&format!("{package}.")))
                {
                    fs::remove_file(entry.path()).map_err(|e| e.to_string())?;
                }
            }
        }
        let mut archive = tar::Archive::new(
            File::open(self.directory().join("payload.tar")).map_err(|e| e.to_string())?,
        );
        let mut metadata = Vec::new();
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let relative = entry.path().map_err(|e| e.to_string())?.into_owned();
            if relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err("invalid snapshot path".into());
            }
            let mut parts = relative.components();
            let root = parts.next().ok_or("empty snapshot path")?.as_os_str();
            let base = if root == "apps" {
                self.layout.apps.join(&self.id)
            } else if root == "data" {
                self.layout.data.join(&self.id)
            } else if root == "info" {
                self.layout.info.clone()
            } else {
                return Err("invalid snapshot root".into());
            };
            let target = base.join(parts.as_path());
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let kind = entry.header().entry_type();
            if kind.is_dir() {
                fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            } else if kind.is_symlink() {
                symlink(
                    entry
                        .link_name()
                        .map_err(|e| e.to_string())?
                        .ok_or("missing snapshot link")?,
                    &target,
                )
                .map_err(|e| e.to_string())?;
            } else if kind.is_file() {
                let mut output = File::create(&target).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut output).map_err(|e| e.to_string())?;
                output.sync_all().map_err(|e| e.to_string())?;
            } else {
                return Err("unsupported snapshot entry".into());
            }
            metadata.push((
                target,
                kind.is_symlink(),
                entry.header().mode().map_err(|e| e.to_string())?,
                entry.header().uid().map_err(|e| e.to_string())? as u32,
                entry.header().gid().map_err(|e| e.to_string())? as u32,
                entry.header().mtime().map_err(|e| e.to_string())?,
            ));
        }
        // Restore permissions before numeric ownership; Core's child may lack
        // CAP_FOWNER. Parents stay accessible until their children are restored.
        for (path, link, mode, uid, gid, mtime) in metadata.into_iter().rev() {
            if !link {
                fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                    .map_err(|e| e.to_string())?;
            }
            let path =
                std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
            let times = [libc::timespec {
                tv_sec: mtime as libc::time_t,
                tv_nsec: 0,
            }; 2];
            if unsafe {
                libc::utimensat(
                    libc::AT_FDCWD,
                    path.as_ptr(),
                    times.as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            if unsafe { libc::lchown(path.as_ptr(), uid, gid) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        let mut restored = status
            .split("\n\n")
            .filter(|part| !part.trim().is_empty() && !packages.contains(package_name(part)))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !self.status.is_empty() {
            restored.push(self.status.clone());
        }
        sys::atomic_write(
            &self.layout.status,
            format!("{}\n\n", restored.join("\n\n")).as_bytes(),
            0o644,
        )
    }
}

pub fn pending(id: &str) -> bool {
    let root = Path::new(ROOT).join(id);
    root.exists() && !root.join("committed").exists()
}

fn has_payload_access() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("CapEff:")
                    .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
            })
        })
        .is_some_and(|caps| caps & 6 != 0)
}

fn load(id: &str) -> Result<Snapshot, String> {
    if !paths::valid_id(id) {
        return Err("invalid extension transaction id".into());
    }
    let path = Path::new(ROOT).join(id).join("journal.json");
    let snapshot: Snapshot = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if snapshot.id != id {
        return Err("transaction identity mismatch".into());
    }
    Ok(snapshot)
}

/// Root-only D-Bus operations run in the host's privilege context, not in
/// Core's deliberately capability-restricted CLI child. No manager-lock
/// acquisition here: the originating mutation already owns that lock.
pub fn payload_operation(id: &str, capture: bool) -> Result<(), String> {
    let mut snapshot = load(id)?;
    if capture {
        if snapshot.prepared {
            return Err("transaction already prepared".into());
        }
        snapshot.capture_local()
    } else if snapshot.prepared {
        snapshot.restore_payload_local()
    } else {
        Err("transaction has no payload snapshot".into())
    }
}

pub fn failed(snapshot: &Snapshot, error: String) -> Result<(), String> {
    crate::install::log_install_message(&format!("Installation failed: {error}"));
    crate::install::log_install_message("Restoring previous extension");
    match snapshot.rollback(true) {
        Ok(()) => {
            crate::install::log_install_message("Previous extension restored");
            Err(format!("{error}; previous extension restored"))
        }
        Err(recovery) => Err(format!(
            "{error}; rollback failed: {recovery}; snapshot retained at {}",
            snapshot.directory().display()
        )),
    }
}

/// Called under the manager lock before a new mutation, or before host startup.
pub fn recover_all(restart: bool) -> Result<(), String> {
    let entries = match fs::read_dir(ROOT) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries {
        let directory = entry.map_err(|e| e.to_string())?.path();
        if directory.join("committed").exists() {
            crate::install::remove_extension_tree(&directory).map_err(|e| e.to_string())?;
            continue;
        }
        let snapshot: Snapshot = serde_json::from_slice(
            &fs::read(directory.join("journal.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if !paths::valid_id(&snapshot.id) || snapshot.directory() != directory {
            return Err("invalid extension recovery journal".into());
        }
        snapshot
            .rollback(restart)
            .map_err(|e| format!("recover {}: {e}", snapshot.id))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payload_operations_remain_exclusive_until_the_owner_releases_its_file() {
        let root =
            std::env::temp_dir().join(format!("onekvm-transaction-lock-{}", std::process::id()));
        let snapshot = Snapshot {
            layout: Layout {
                root: root.clone(),
                ..Layout::default()
            },
            id: "probe".into(),
            state: PluginState::default(),
            state_exists: false,
            prepared: false,
            packages: Default::default(),
            status: String::new(),
        };
        fs::create_dir_all(snapshot.directory()).unwrap();
        let first = snapshot.payload_lock().unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(snapshot.directory().join("payload.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        drop(first);
        assert_eq!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        drop(second);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn same_version_rollback_restores_payload_data_and_only_its_package_records() {
        let root = std::env::temp_dir().join(format!("onekvm-transaction-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let layout = Layout {
            root: root.join("transactions"),
            apps: root.join("apps"),
            data: root.join("data"),
            info: root.join("info"),
            status: root.join("status"),
        };
        fs::create_dir_all(layout.apps.join("example/1.0")).unwrap();
        fs::create_dir_all(layout.data.join("example")).unwrap();
        fs::create_dir_all(&layout.info).unwrap();
        fs::write(layout.apps.join("example/1.0/main"), b"old payload").unwrap();
        fs::write(layout.data.join("example/database"), b"old data").unwrap();
        symlink("database", layout.data.join("example/link")).unwrap();
        fs::write(
            layout.info.join("onekvm-extension-example.list"),
            b"old package list",
        )
        .unwrap();
        let original =
            "Package: onekvm-extension-example\nVersion: 1.0\nStatus: install ok installed";
        let neighbor = "Package: other\nVersion: 2.0\nStatus: install ok installed";
        fs::write(&layout.status, format!("{original}\n\n{neighbor}\n\n")).unwrap();
        let mut snapshot = Snapshot {
            layout: layout.clone(),
            id: "example".into(),
            state: PluginState::default(),
            state_exists: false,
            prepared: false,
            packages: ["onekvm-extension-example".into()].into(),
            status: original.into(),
        };
        fs::create_dir_all(snapshot.directory()).unwrap();
        snapshot.capture_local().unwrap();
        fs::write(
            layout.apps.join("example/1.0/main"),
            b"broken same-version replacement",
        )
        .unwrap();
        fs::write(
            layout.data.join("example/database"),
            b"incompatible migration",
        )
        .unwrap();
        fs::write(
            layout.info.join("onekvm-extension-example.list"),
            b"new package list",
        )
        .unwrap();
        fs::write(
            &layout.status,
            format!("Package: onekvm-extension-example\nVersion: 1.1\n\n{neighbor}\n\n"),
        )
        .unwrap();
        snapshot.restore_payload_local().unwrap();
        assert_eq!(
            fs::read(layout.apps.join("example/1.0/main")).unwrap(),
            b"old payload"
        );
        assert_eq!(
            fs::read(layout.data.join("example/database")).unwrap(),
            b"old data"
        );
        assert_eq!(
            fs::read_link(layout.data.join("example/link")).unwrap(),
            PathBuf::from("database")
        );
        assert_eq!(
            fs::read(layout.info.join("onekvm-extension-example.list")).unwrap(),
            b"old package list"
        );
        let status = fs::read_to_string(&layout.status).unwrap();
        assert!(status.contains(original));
        assert!(status.contains(neighbor));
        assert!(!status.contains("Version: 1.1"));
        fs::remove_dir_all(root).unwrap();
    }
}
