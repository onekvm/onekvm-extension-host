//! Runtime images use a system service for loop setup. Plugin units keep their
//! existing sandbox and borrow only the validated tree via ExtensionDirectories.
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::manifest::{Installed, Manifest};
use crate::{paths, sandbox};

const IMAGE_ROOT: &str = "/run/onekvm/extension-images";
const AUTHORIZATIONS: &str = ".runtime-image-authorizations";
const PRIVATE_USR: &str = "usr/lib/onekvm/extensions";
const MAX_RELEASE: u64 = 4096;

extern "C" {
    fn onekvm_mount_runtime_image(image: libc::c_int, target: *const libc::c_char) -> libc::c_int;
}

pub fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    let Some(image) = &manifest.runtime_image else {
        return Ok(());
    };
    if manifest.api_version != 2
        || manifest.architecture.is_empty()
        || image.path != format!("runtime/{}.raw", manifest.id)
        || !valid_digest(&image.sha256)
    {
        return Err("runtime_image requires an API v2 architecture, runtime/<id>.raw and a lowercase SHA-256".into());
    }
    if manifest.permissions.iter().any(|permission| {
        matches!(
            permission.as_str(),
            "host-root" | "host-unrestricted" | "container-admin"
        )
    }) || manifest
        .services
        .iter()
        .any(|service| !service.unit.is_empty())
    {
        return Err("runtime_image requires extension host managed sandboxed processes".into());
    }
    if manifest.entrypoint.is_empty()
        && manifest.controller.is_none()
        && !manifest.methods.has_commands()
    {
        return Err(
            "runtime_image requires an executable entrypoint, controller or command method".into(),
        );
    }
    Ok(())
}

struct Instance<'a> {
    id: &'a str,
    version: String,
    digest: &'a str,
}

impl<'a> Instance<'a> {
    fn parse(key: &'a str) -> Result<Self, String> {
        let (id, rest) = key
            .split_once(':')
            .ok_or("invalid runtime image instance")?;
        let (version, digest) = rest
            .rsplit_once(':')
            .ok_or("invalid runtime image instance")?;
        let version = version.replace(':', "+");
        if !paths::valid_id(id) || !paths::valid_version(&version) || !valid_digest(digest) {
            return Err("invalid runtime image instance".into());
        }
        Ok(Self {
            id,
            version,
            digest,
        })
    }

    fn for_manifest(manifest: &'a Manifest) -> Option<Self> {
        manifest.runtime_image.as_ref().map(|image| Self {
            id: &manifest.id,
            version: manifest.version.clone(),
            digest: &image.sha256,
        })
    }

    fn for_extension(extension: &'a Installed) -> Option<Self> {
        Self::for_manifest(&extension.manifest)
    }

    fn key(&self) -> String {
        // Unit instances accept ':' but not '+'. IDs and digests have neither,
        // so splitting at the first and last ':' recovers the complete version.
        format!(
            "{}:{}:{}",
            self.id,
            self.version.replace('+', ":"),
            self.digest
        )
    }

    fn unit(&self) -> String {
        format!("onekvm-extension-image@{}.service", self.key())
    }

    fn tree(&self, check: bool) -> PathBuf {
        let key = self.key_digest();
        let directory = if check { format!("check-{key}") } else { key };
        Path::new(IMAGE_ROOT).join(directory).join(self.id)
    }

    fn key_digest(&self) -> String {
        format!("{:x}", Sha256::digest(self.key()))
    }
}

/// A package cannot write above its own extension directory. Keep the host's
/// authorization beside those directories so older, unsigned installations
/// cannot acquire mount privileges by declaring a previously ignored field.
pub fn record_authenticated(manifest: &Manifest) -> Result<(), String> {
    let Some(instance) = Instance::for_manifest(manifest) else {
        return Ok(());
    };
    protected_apps_root()?;
    let base = Path::new(paths::APPS_ROOT).join(AUTHORIZATIONS);
    let directory = base.join(instance.id);
    for path in [&base, &directory] {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create runtime image authorization: {error}")),
        }
        checked_directory(path)
            .map_err(|error| format!("check runtime image authorization: {error}"))?;
    }
    crate::sys::atomic_write(
        &directory.join(instance.key_digest()),
        instance.key().as_bytes(),
        0o600,
    )
}

pub fn forget_authenticated(id: &str) -> Result<(), String> {
    let directory = Path::new(paths::APPS_ROOT).join(AUTHORIZATIONS).join(id);
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove runtime image authorizations: {error}")),
    }
}

pub fn append_properties(extension: &Installed, arguments: &mut Vec<String>) {
    if let Some(instance) = Instance::for_extension(extension) {
        let unit = instance.unit();
        arguments.extend([
            format!("--property=BindsTo={unit}"),
            format!("--property=After={unit}"),
            format!(
                "--property=ExtensionDirectories={}",
                instance.tree(false).display()
            ),
        ]);
    }
}

/// An enabled extension with a successful exec health check already used a
/// validated mount. Disabled and cold socket services need a separate check.
pub fn validate_install(extension: &Installed, enabled: bool) -> Result<(), String> {
    let Some(instance) = Instance::for_extension(extension) else {
        return Ok(());
    };
    if (enabled && extension.manifest.healthcheck.type_name == "exec")
        || sandbox::is_active(&instance.unit())
    {
        return Ok(());
    }
    let unit = format!("onekvm-extension-image-check@{}.service", instance.key());
    let result = sandbox::unit_action("restart", &unit);
    let stop = sandbox::unit_action("stop", &unit);
    result.and(stop).map_err(|error| {
        format!(
            "runtime image for {} failed validation ({error}); see journal for {unit}",
            instance.id
        )
    })
}

pub fn command(arguments: &[String]) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("runtime image setup requires root".into());
    }
    let [operation, key] = arguments else {
        return Err("invalid runtime image operation".into());
    };
    let instance = Instance::parse(key)?;
    let result = match operation.as_str() {
        "mount" => mount_image(&instance, false),
        "unmount" => release_tree(&instance.tree(false)),
        "check" => check_image(&instance),
        "clean-check" => release_tree(&instance.tree(true)),
        _ => Err("invalid runtime image operation".into()),
    };
    result.map_err(|error| {
        format!(
            "runtime image {}/{}: {error}",
            instance.id, instance.version
        )
    })
}

fn check_image(instance: &Instance<'_>) -> Result<(), String> {
    // A private check releases its mount even if the validation process dies.
    let root = c"/";
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } < 0
        || unsafe {
            libc::mount(
                std::ptr::null(),
                root.as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } < 0
    {
        return Err(format!(
            "isolate image validation: {}",
            io::Error::last_os_error()
        ));
    }
    let result = mount_image(instance, true);
    let release = release_tree(&instance.tree(true));
    result.and(release)
}

fn c_path(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| "NUL in runtime image path".into())
}

fn check_owner(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.uid() != 0 || metadata.gid() != 0 || metadata.mode() & 0o7022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe runtime image ownership or permissions",
        ));
    }
    Ok(())
}

fn protected_open(parent: &File, name: &str, directory: bool) -> io::Result<File> {
    let name = CString::new(name)?;
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | if directory {
            libc::O_DIRECTORY | libc::O_RDONLY
        } else {
            libc::O_PATH
        };
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { File::from_raw_fd(fd) };
    let meta = fd.metadata()?;
    check_owner(&meta)?;
    if directory {
        return Ok(fd);
    }
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular runtime image file with one link",
        ));
    }
    File::open(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

fn open_protected(parent: &File, name: &str, directory: bool) -> Result<File, String> {
    protected_open(parent, name, directory).map_err(|error| format!("open {name}: {error}"))
}

fn protected_apps_root() -> Result<File, String> {
    let mut parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")
        .map_err(|error| error.to_string())?;
    for component in Path::new(paths::APPS_ROOT).iter().skip(1) {
        parent = open_protected(
            &parent,
            component.to_str().ok_or("invalid extension root")?,
            true,
        )?;
    }
    Ok(parent)
}

fn open_image(instance: &Instance<'_>) -> Result<File, String> {
    let mut parent = protected_apps_root()?;
    let authorizations = open_protected(&parent, AUTHORIZATIONS, true)?;
    let authorized = open_protected(&authorizations, instance.id, true)?;
    let ticket = open_protected(&authorized, &instance.key_digest(), false)
        .map_err(|error| format!("runtime image has no publisher authorization: {error}"))?;
    let mut key = Vec::new();
    ticket
        .take(256)
        .read_to_end(&mut key)
        .map_err(|error| format!("read runtime image authorization: {error}"))?;
    if key != instance.key().as_bytes() {
        return Err("runtime image publisher authorization does not match".into());
    }
    for component in [instance.id, instance.version.as_str()] {
        parent = open_protected(&parent, component, true)?;
    }
    let mut data = Vec::new();
    open_protected(&parent, "manifest.json", false)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("read installed manifest: {error}"))?;
    if data.len() > 64 * 1024 {
        return Err("installed manifest is too large".into());
    }
    let manifest = crate::manifest::decode_manifest(&data)?;
    if manifest.id != instance.id || manifest.version != instance.version {
        return Err("installed manifest does not match the runtime image instance".into());
    }
    let image = manifest
        .runtime_image
        .ok_or("installed manifest has no runtime_image")?;
    if image.sha256 != instance.digest {
        return Err("installed runtime image digest does not match the requested instance".into());
    }
    let directory = open_protected(&parent, "runtime", true)?;
    let mut image = open_protected(&directory, &format!("{}.raw", instance.id), false)?;
    if image.metadata().map_err(|error| error.to_string())?.mode() & 0o7777 != 0o444 {
        return Err("runtime image must have mode 0444".into());
    }
    let mut magic = [0; 4];
    image
        .read_exact(&mut magic)
        .map_err(|error| format!("read runtime image: {error}"))?;
    if &magic != b"hsqs" {
        return Err("runtime image must be a SquashFS filesystem".into());
    }
    let mut sha = Sha256::new();
    sha.update(magic);
    io::copy(&mut image, &mut sha).map_err(|error| format!("hash runtime image: {error}"))?;
    if format!("{:x}", sha.finalize()) != instance.digest {
        return Err("runtime image SHA-256 mismatch".into());
    }
    Ok(image)
}

fn checked_directory(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    check_owner(&metadata)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a root-owned directory",
        ));
    }
    Ok(metadata)
}

fn mount_image(instance: &Instance<'_>, check: bool) -> Result<(), String> {
    let image = open_image(instance)?;
    let root = Path::new(IMAGE_ROOT);
    checked_directory(root.parent().unwrap())
        .map_err(|error| format!("check runtime directory: {error}"))?;
    let tree = instance.tree(check);
    for directory in [root, tree.parent().unwrap(), tree.as_path()] {
        match fs::create_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create {}: {error}", directory.display())),
        }
        checked_directory(directory)
            .map_err(|error| format!("check {}: {error}", directory.display()))?;
    }
    if mounted(&tree)? {
        return Err("runtime image mountpoint is occupied".into());
    }
    let target = c_path(&tree)?;
    let result = unsafe { onekvm_mount_runtime_image(image.as_raw_fd(), target.as_ptr()) };
    if result < 0 {
        return Err(format!(
            "mount read-only SquashFS: {}",
            io::Error::from_raw_os_error(-result)
        ));
    }
    validate_tree(&tree, instance.id)
}

fn mounted(path: &Path) -> Result<bool, String> {
    let parent = path.parent().unwrap();
    match checked_directory(parent) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    // The visible root of a rejected filesystem can itself have unsafe
    // metadata. It must still be unmounted from our protected parent.
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(metadata.dev()
            != checked_directory(parent)
                .map_err(|error| error.to_string())?
                .dev()),
        Ok(_) => Err("runtime image mountpoint is not a directory".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

pub fn startup_error(extension: &Installed, error: String) -> String {
    match Instance::for_extension(extension) {
        Some(instance) => format!("{error}; see journal for {}", instance.unit()),
        None => error,
    }
}

pub fn stop(extension: &Installed) -> Result<(), String> {
    if let Some(instance) = Instance::for_extension(extension) {
        sandbox::unit_action("stop", &instance.unit())?;
    }
    Ok(())
}

fn release_tree(tree: &Path) -> Result<(), String> {
    if mounted(tree)? {
        let path = c_path(tree)?;
        // The helper can still hold a lower-layer reference while its namespace
        // exits. Detach here; loop AUTOCLEAR frees the last reference later.
        if unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) } < 0 {
            return Err(format!(
                "unmount runtime image: {}",
                io::Error::last_os_error()
            ));
        }
    }
    for directory in [tree, tree.parent().unwrap()] {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", directory.display())),
        }
    }
    Ok(())
}

fn no_xattrs(path: &Path) -> Result<(), String> {
    let path = c_path(path)?;
    let size = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if size == 0 || (size < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSUP)) {
        return Ok(());
    }
    Err("runtime image entries may not carry extended attributes".into())
}

fn validate_tree(tree: &Path, id: &str) -> Result<(), String> {
    let own = Path::new(PRIVATE_USR).join(id);
    let metadata = Path::new("usr/lib/extension-release.d").join(format!("extension-release.{id}"));
    checked_directory(tree).map_err(|error| error.to_string())?;
    no_xattrs(tree)?;
    let mut pending = vec![PathBuf::new()];
    let mut runtime_dirs = false;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(tree.join(&directory)).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let relative = directory.join(entry.file_name());
            let relative_display = relative.display();
            let meta = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
            check_owner(&meta).map_err(|error| format!("{relative_display}: {error}"))?;
            no_xattrs(&entry.path()).map_err(|error| format!("{relative_display}: {error}"))?;
            let component = relative
                .strip_prefix(&own)
                .ok()
                .and_then(|path| path.iter().next());
            let allowed = component.is_some_and(|name| {
                ["bin", "lib", "share"]
                    .iter()
                    .any(|part| name == OsStr::new(part))
            });
            if meta.is_dir()
                && (own.starts_with(&relative) || metadata.starts_with(&relative) || allowed)
            {
                if meta.mode() & 0o555 != 0o555 {
                    return Err(format!("runtime image directory {relative_display} must be readable and traversable"));
                }
                if directory == own {
                    runtime_dirs = true;
                }
                pending.push(relative);
            } else if !(meta.is_file()
                && meta.nlink() == 1
                && (relative == metadata || (allowed && relative.parent() != Some(own.as_path()))))
            {
                return Err(format!("unexpected runtime image entry {relative_display}"));
            }
        }
    }
    if !runtime_dirs {
        return Err(format!(
            "runtime image contains no bin, lib or share directory under {}",
            own.display()
        ));
    }
    let mut release = String::new();
    File::open(tree.join(metadata))
        .map_err(|error| format!("read extension-release: {error}"))?
        .take(MAX_RELEASE + 1)
        .read_to_string(&mut release)
        .map_err(|error| format!("read extension-release: {error}"))?;
    if release.len() as u64 > MAX_RELEASE {
        return Err("runtime image extension-release is too large".into());
    }
    validate_release(&release)
}

fn validate_release(release: &str) -> Result<(), String> {
    let host = fs::read_to_string("/etc/os-release")
        .or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                fs::read_to_string("/usr/lib/os-release")
            } else {
                Err(error)
            }
        })
        .map_err(|error| format!("read system os-release: {error}"))?;
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "x86-64",
        "aarch64" => "arm64",
        architecture => architecture,
    };
    validate_release_for(release, &host, architecture)
}

fn validate_release_for(release: &str, host: &str, architecture: &str) -> Result<(), String> {
    let host: BTreeMap<_, _> = host
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key, value.trim_matches(['\'', '"'])))
        .collect();
    let mut actual = BTreeMap::new();
    for line in release
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (key, value) = line
            .split_once('=')
            .ok_or("invalid runtime image extension-release line")?;
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        if !matches!(key, "ID" | "VERSION_ID" | "ARCHITECTURE" | "SYSEXT_LEVEL")
            || value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte))
            || actual.insert(key, value).is_some()
        {
            return Err(format!(
                "invalid runtime image extension-release field {key}"
            ));
        }
    }
    if actual.get("ARCHITECTURE") != Some(&architecture)
        || host.get("ID").is_none_or(|id| actual.get("ID") != Some(id))
    {
        return Err("runtime image extension-release OS ID or architecture is incompatible".into());
    }
    let compatibility = if host.contains_key("SYSEXT_LEVEL") && actual.contains_key("SYSEXT_LEVEL")
    {
        "SYSEXT_LEVEL"
    } else {
        "VERSION_ID"
    };
    if host
        .get(compatibility)
        .is_none_or(|value| actual.get(compatibility) != Some(value))
    {
        return Err(format!(
            "runtime image extension-release {compatibility} is incompatible"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::RuntimeImage;

    #[test]
    fn image_properties_are_opt_in_and_bound_to_content_and_version() {
        let mut extension = Installed {
            manifest: Manifest {
                id: "image-example".into(),
                version: "1.2.3+debug".into(),
                ..Default::default()
            },
            root: PathBuf::new(),
        };
        let mut arguments = Vec::new();
        append_properties(&extension, &mut arguments);
        assert!(arguments.is_empty());
        extension.manifest.runtime_image = Some(RuntimeImage {
            path: "runtime/image-example.raw".into(),
            sha256: "1234".repeat(16),
        });
        let instance = Instance::for_extension(&extension).unwrap();
        let key = instance.key();
        let parsed = Instance::parse(&key).unwrap();
        assert_eq!(parsed.id, extension.manifest.id);
        assert_eq!(parsed.version, extension.manifest.version);
        assert_eq!(parsed.tree(false).file_name().unwrap(), parsed.id);
        assert_ne!(parsed.tree(false), parsed.tree(true));
        append_properties(&extension, &mut arguments);
        assert!(arguments.contains(&format!("--property=BindsTo={}", parsed.unit())));
        assert!(arguments.contains(&format!(
            "--property=ExtensionDirectories={}",
            parsed.tree(false).display()
        )));
        extension.manifest.runtime_image.as_mut().unwrap().sha256 = "5678".repeat(16);
        assert_ne!(
            Instance::for_extension(&extension).unwrap().tree(false),
            parsed.tree(false)
        );
    }

    #[test]
    fn image_manifest_rejects_traversal_uppercase_and_privileged_sandboxes() {
        let mut manifest = Manifest {
            id: "image-example".into(),
            api_version: 2,
            architecture: crate::manifest::runtime_architecture(),
            entrypoint: "bin/launch".into(),
            runtime_image: Some(RuntimeImage {
                path: "runtime/image-example.raw".into(),
                sha256: "9abc".repeat(16),
            }),
            ..Default::default()
        };
        assert!(validate_manifest(&manifest).is_ok());
        manifest.permissions.push("host-root".into());
        assert!(validate_manifest(&manifest).is_err());
        manifest.permissions.clear();
        manifest
            .runtime_image
            .as_mut()
            .unwrap()
            .sha256
            .make_ascii_uppercase();
        assert!(validate_manifest(&manifest).is_err());
        manifest
            .runtime_image
            .as_mut()
            .unwrap()
            .sha256
            .make_ascii_lowercase();
        manifest.runtime_image.as_mut().unwrap().path = "../image-example.raw".into();
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn image_release_requires_one_compatible_identity() {
        let host = "ID=onekvm\nVERSION_ID=\"0.2.0\"\n";
        let release = "ID=onekvm\nVERSION_ID=0.2.0\nARCHITECTURE=riscv64\n";
        assert!(validate_release_for(release, host, "riscv64").is_ok());
        assert!(validate_release_for(release, host, "arm64").is_err());
        assert!(validate_release_for(release, "ID=onekvm\nVERSION_ID=0.3.0\n", "riscv64").is_err());
        assert!(validate_release_for(&format!("{release}ID=onekvm\n"), host, "riscv64").is_err());
    }
}
