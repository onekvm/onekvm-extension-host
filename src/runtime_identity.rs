use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{manifest::Manifest, paths, state, sys};

pub const PACKAGE_CAPABILITY: &str = "onekvm-extension-dedicated-account";
const REGISTRY: &str = "/data/pkg-state/extension-accounts";
const FIRST_SYSTEM_ID: u32 = 600;
const LAST_SYSTEM_ID: u32 = 699;
const HOME: &str = "/nonexistent";
const SHELL: &str = "/sbin/nologin";

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Declaration {
    Dedicated,
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.runtime_identity.is_some()
        && (manifest.api_version != 2
            || manifest.is_environment()
            || manifest
                .services
                .iter()
                .any(|service| !service.unit.is_empty())
            || manifest.permissions.iter().any(|permission| {
                matches!(
                    permission.as_str(),
                    "host-root" | "host-unrestricted" | "container-admin"
                )
            }))
    {
        return Err("runtime_identity requires a host-managed unprivileged API v2 runtime".into());
    }
    Ok(())
}

#[derive(Debug)]
pub struct Account {
    pub name: String,
    pub id: u32,
}

#[derive(Deserialize, Serialize)]
struct Record {
    extension: String,
    id: u32,
}

fn account_name(extension: &str) -> String {
    // Limit the Linux name to 31 bytes. The registry and the passwd entry
    // independently bind it to the complete, untruncated extension ID.
    let digest = format!("{:x}", Sha256::digest(extension.as_bytes()));
    format!("okv-{}", &digest[..27])
}

fn record_path(extension: &str) -> std::path::PathBuf {
    Path::new(REGISTRY).join(account_name(extension))
}

pub fn is_managed(manifest: &Manifest) -> bool {
    manifest.runtime_identity.is_some() || record_path(&manifest.id).exists()
}

struct Accounts {
    passwd: String,
    group: String,
}

impl Accounts {
    fn load() -> Result<Self, String> {
        Ok(Self {
            passwd: fs::read_to_string("/etc/passwd").map_err(|e| format!("read passwd: {e}"))?,
            group: fs::read_to_string("/etc/group").map_err(|e| format!("read group: {e}"))?,
        })
    }

    fn validate(&self, account: &Account, extension: &str) -> Result<(bool, bool), String> {
        let id = account.id.to_string();
        let comment = format!("OneKVM extension {extension}");
        let conflict = || format!("conflicting system account for extension {extension}");
        let mut user = false;
        for line in self.passwd.lines() {
            let fields: Vec<_> = line.split(':').collect();
            let [name, _, uid, gid, gecos, home, shell] = fields.as_slice() else {
                continue;
            };
            if *name != account.name && *uid != id && *gid != id {
                continue;
            }
            if user
                || *name != account.name
                || *uid != id
                || *gid != id
                || *gecos != comment
                || *home != HOME
                || *shell != SHELL
            {
                return Err(conflict());
            }
            user = true;
        }
        let mut group = false;
        for line in self.group.lines() {
            let fields: Vec<_> = line.split(':').collect();
            let [name, _, gid, members] = fields.as_slice() else {
                continue;
            };
            if *name == account.name || *gid == id {
                if group || *name != account.name || *gid != id || !members.is_empty() {
                    return Err(conflict());
                }
                group = true;
            } else if members.split(',').any(|member| member == account.name) {
                return Err(conflict());
            }
        }
        Ok((user, group))
    }
}

fn read_record(path: &Path) -> Result<Option<Record>, String> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| format!("read extension account registry: {e}")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read extension account registry: {e}")),
    }
}

fn reserve(extension: &str, accounts: &Accounts) -> Result<Record, String> {
    let name = account_name(extension);
    let mut occupied = std::collections::BTreeSet::new();
    for line in accounts.passwd.lines().chain(accounts.group.lines()) {
        let mut fields = line.split(':');
        if fields.next() == Some(&name) {
            return Err(format!(
                "unregistered system account for extension {extension}"
            ));
        }
        if let Some(id) = fields.nth(1).and_then(|field| field.parse::<u32>().ok()) {
            occupied.insert(id);
        }
        if let Some(primary_gid) = fields.next().and_then(|field| field.parse::<u32>().ok()) {
            occupied.insert(primary_gid);
        }
    }
    for entry in fs::read_dir(REGISTRY).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_name().as_bytes().starts_with(b"okv-") {
            if let Some(record) = read_record(&entry.path())? {
                occupied.insert(record.id);
            }
        }
    }
    let id = (FIRST_SYSTEM_ID..=LAST_SYSTEM_ID)
        .rev()
        .find(|id| !occupied.contains(id))
        .ok_or("no system UID/GID available for dedicated extensions")?;
    let record = Record {
        extension: extension.into(),
        id,
    };
    let data = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
    // Reserve before invoking shadow so an interrupted creation can be resumed.
    sys::atomic_write(&record_path(extension), &data, 0o600)?;
    Ok(record)
}

fn shadow(command: &str, arguments: &[&str]) -> Result<(), String> {
    let output = Command::new(command)
        .args(arguments)
        .output()
        .map_err(|e| format!("{command}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn account(manifest: &Manifest) -> Result<Option<Account>, String> {
    let path = record_path(&manifest.id);
    let record = match read_record(&path)? {
        Some(record) => record,
        None if manifest.runtime_identity.is_some() => reserve(&manifest.id, &Accounts::load()?)?,
        None => return Ok(None),
    };
    if record.extension != manifest.id || !(FIRST_SYSTEM_ID..=LAST_SYSTEM_ID).contains(&record.id) {
        return Err(format!(
            "conflicting account registration for extension {}",
            manifest.id
        ));
    }
    let account = Account {
        name: account_name(&manifest.id),
        id: record.id,
    };
    let (user, group) = Accounts::load()?.validate(&account, &manifest.id)?;
    if manifest.runtime_identity.is_none() || (user && group) {
        return Ok(Some(account));
    }
    let number = account.id.to_string();
    if !group {
        shadow("groupadd", &["--system", "--gid", &number, &account.name])?;
    }
    if !user {
        let comment = format!("OneKVM extension {}", manifest.id);
        shadow(
            "useradd",
            &[
                "--system",
                "--no-user-group",
                "--no-create-home",
                "--no-log-init",
                "--uid",
                &number,
                "--gid",
                &number,
                "--comment",
                &comment,
                "--home-dir",
                HOME,
                "--shell",
                SHELL,
                &account.name,
            ],
        )?;
    }
    if Accounts::load()?.validate(&account, &manifest.id)? != (true, true) {
        return Err(format!(
            "could not provision system account for {}",
            manifest.id
        ));
    }
    Ok(Some(account))
}

pub fn prepare(
    manifest: &Manifest,
    quiesce: impl FnOnce() -> Result<(), String>,
) -> Result<Option<Account>, String> {
    if !is_managed(manifest) {
        state::prepare_runtime_dirs(&manifest.id)?;
        return Ok(None);
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(REGISTRY)
        .map_err(|e| format!("create extension account registry: {e}"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(Path::new(REGISTRY).join("lock"))
        .map_err(|e| format!("open extension account lock: {e}"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "lock extension accounts: {}",
            io::Error::last_os_error()
        ));
    }
    let Some(account) = account(manifest)? else {
        state::prepare_runtime_dirs(&manifest.id)?;
        return Ok(None);
    };
    let shared = sys::runtime_ids()?;
    let dedicated = (account.id, account.id);
    let (source, target) = if manifest.runtime_identity.is_some() {
        (shared, dedicated)
    } else {
        (dedicated, shared)
    };
    let data = Path::new(paths::APP_DATA_ROOT).join(&manifest.id);
    let runtime = Path::new(paths::RUNTIME_ROOT).join(&manifest.id);
    let sockets = runtime.join("sockets");
    let owner = |path: &Path| -> Result<Option<u32>, String> {
        match fs::symlink_metadata(path) {
            Ok(info) if info.is_dir() => Ok(Some(info.uid())),
            Ok(_) => Err(format!(
                "extension directory is not a directory: {}",
                path.display()
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!(
                "inspect extension directory {}: {e}",
                path.display()
            )),
        }
    };
    owner(&runtime)?;
    let data_owner = owner(&data)?;
    let socket_owner = owner(&sockets)?;
    if [data_owner, socket_owner]
        .into_iter()
        .flatten()
        .any(|uid| uid != target.0)
    {
        quiesce()?;
        for (path, uid) in [(&data, data_owner), (&sockets, socket_owner)] {
            if uid.is_some_and(|uid| uid != target.0) {
                migrate(path, source.0, target)
                    .map_err(|e| format!("migrate {}: {e}", path.display()))?;
            }
        }
    }
    state::prepare_runtime_dirs_as(&manifest.id, target.0, target.1)?;
    Ok(manifest.runtime_identity.map(|_| account))
}

fn fd_path(file: &File) -> std::path::PathBuf {
    format!("/proc/self/fd/{}", file.as_raw_fd()).into()
}

fn set_owner(file: &File, owner: (u32, u32)) -> io::Result<()> {
    let result = unsafe {
        libc::fchownat(
            file.as_raw_fd(),
            c"".as_ptr(),
            owner.0,
            owner.1,
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn migrate(path: &Path, source: u32, target: (u32, u32)) -> io::Result<()> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let owner = dir.metadata()?.uid();
    if owner != 0 && owner != source {
        return Err(io::Error::other(
            "directory belongs to an unrelated account",
        ));
    }
    // Seal the top-level directory before walking it. Keep it root-owned
    // until the new ownership has reached disk so retries repeat interrupted
    // migrations and restored snapshots can be identified by their owner.
    set_owner(&dir, (0, 0))?;
    dir.set_permissions(fs::Permissions::from_mode(0o700))?;
    migrate_children(&dir, source, target)?;
    if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    dir.set_permissions(fs::Permissions::from_mode(0o750))?;
    set_owner(&dir, target)
}

fn open_child(dir: &File, name: &OsStr) -> io::Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let name = CString::new(name.as_bytes())?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: 0x01 | 0x04, // RESOLVE_NO_XDEV | RESOLVE_NO_SYMLINKS
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            name.as_ptr(),
            &how,
            size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

fn migrate_children(dir: &File, source: u32, target: (u32, u32)) -> io::Result<()> {
    for entry in fs::read_dir(fd_path(dir))? {
        let entry = entry?;
        let child = open_child(dir, &entry.file_name())?;
        let info = child.metadata()?;
        if info.is_dir() && (info.uid() == source || info.uid() == target.0) {
            migrate_children(&child, source, target)?;
        }
        if info.uid() == source {
            if !info.is_dir() && info.nlink() != 1 {
                return Err(io::Error::other(
                    "cannot migrate multiply linked extension data",
                ));
            }
            set_owner(&child, target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_name_binds_the_complete_extension_id() {
        let prefix = "a".repeat(62);
        let first = account_name(&format!("{prefix}a"));
        let second = account_name(&format!("{prefix}b"));
        assert_eq!(first.len(), 31);
        assert_ne!(first, second);
    }

    #[test]
    fn private_identity_rejects_account_and_group_aliases() {
        let extension = "test-runtime";
        let name = account_name(extension);
        let account = Account {
            name: name.clone(),
            id: 698,
        };
        let mut db = Accounts {
            passwd: format!("{name}:x:698:698:OneKVM extension {extension}:{HOME}:{SHELL}\n"),
            group: format!("{name}:x:698:\n"),
        };
        assert_eq!(db.validate(&account, extension), Ok((true, true)));
        assert!(db.validate(&account, "other-runtime").is_err());
        db.group.push_str(&format!("privileged:x:200:{name}\n"));
        assert!(db.validate(&account, extension).is_err());
        db.group = format!("{name}:x:698:\n");
        db.passwd.push_str("other:x:1000:698::/:/bin/sh\n");
        assert!(db.validate(&account, extension).is_err());
    }

    #[test]
    fn migration_preserves_bytes_and_host_files_in_both_directions() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let root = std::env::temp_dir().join(format!("onekvm-account-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let data = root.join("data");
        let legacy = (498, 498);
        let dedicated = (698, 698);
        fs::create_dir_all(data.join("vpn")).unwrap();
        let state = data.join("vpn/session");
        fs::write(&state, b"paired fixture\0\xff").unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
        let host_file = data.join("host-owned");
        fs::write(&host_file, b"host metadata").unwrap();
        let external = root.join("external");
        fs::write(&external, b"outside").unwrap();
        std::os::unix::fs::symlink(&external, data.join("link")).unwrap();
        let chown = |path: &Path| {
            let fd = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
                .open(path)
                .unwrap();
            set_owner(&fd, legacy).unwrap();
        };
        for path in [
            &data,
            &data.join("vpn"),
            &state,
            &data.join("link"),
            &external,
        ] {
            chown(path);
        }
        fs::hard_link(&external, data.join("hardlink")).unwrap();
        assert!(migrate(&data, legacy.0, dedicated).is_err());
        assert_eq!(fs::metadata(&data).unwrap().uid(), 0);
        assert_eq!(fs::metadata(&external).unwrap().uid(), legacy.0);
        fs::remove_file(data.join("hardlink")).unwrap();
        for (source, target) in [(legacy, dedicated), (dedicated, legacy)] {
            migrate(&data, source.0, target).unwrap();
            assert_eq!(fs::read(&state).unwrap(), b"paired fixture\0\xff");
            let info = fs::metadata(&state).unwrap();
            assert_eq!(
                (info.uid(), info.gid(), info.mode() & 0o7777),
                (target.0, target.1, 0o600)
            );
            assert_eq!(fs::metadata(&host_file).unwrap().uid(), 0);
            assert_eq!(fs::metadata(&external).unwrap().uid(), legacy.0);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
