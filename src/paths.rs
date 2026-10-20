use std::fs;
use std::path::{Path, PathBuf};

pub const APPS_ROOT: &str = "/data/onekvm/apps";
pub const APP_DATA_ROOT: &str = "/data/onekvm/app-data";
pub const CONFIG_ROOT: &str = "/etc/onekvm/extensions";
pub const RUNTIME_ROOT: &str = "/run/onekvm/extensions";
pub const AUTH_ROOT: &str = "/run/onekvm/extension-auth";
pub const LOCK_PATH: &str = "/run/onekvm/extension-manager.lock";
pub const CONTRIBUTIONS_GENERATION: &str = "/run/onekvm/contributions-generation";
pub const GENERATION_PATH: &str = "/run/onekvm/extension-host-generation";
pub const MACHINE_PROFILE: &str = "/usr/share/onekvm/machine.json";
pub const MACHINE_HARDWARE: &str = "/run/onekvm/hardware.json";
pub const SYSTEM_PLUGINS: &str = "/usr/share/onekvm/system-plugins";
pub const OPKG_CONF_DIR: &str = "/etc/opkg";
/// System/default feed(s). Marketplace custom sources use `onekvm-ext-{key}.conf`.
pub const FEED_CONFIG: &str = "/etc/opkg/onekvm-feed.conf";
pub const FEED_CUSTOM_PREFIX: &str = "onekvm-ext-";
pub const FEED_CUSTOM_SUFFIX: &str = ".conf";
pub const FEED_DISABLED_SUFFIX: &str = ".disabled";
pub const BUILTIN_FEED_NAME: &str = "onekvm";
pub const TRUSTED_FEEDS: &str = "/usr/share/onekvm/trusted-feed-prefixes";
pub const OPKG_LISTS: &str = "/data/pkg-state/opkg/lists";
pub const OPKG_CACHE: &str = "/data/pkg-state/opkg/cache";
pub const OPKG_TMP: &str = "/data/pkg-state/opkg/tmp";
pub const OPKG_SLOTS: &str = "/data/pkg-state/slots";
pub const EXTENSION_PACKAGE_METADATA: &str = "/data/pkg-state/extensions";
pub const UPLOAD_ROOT: &str = "/var/tmp/onekvm/extension-uploads";
pub const LEGACY_CONFIG: &str = "/var/lib/onekvm/extensions/config";
pub const LEGACY_STATE: &str = "/var/lib/onekvm/extensions/state";
pub const CATALOG_CACHE: &str = "/data/onekvm/plugin-catalog.json";
pub const MARKETPLACE_CACHE: &str = "/data/onekvm/plugin-marketplace.json";
pub const ONEKVM_CONFIG: &str = "/etc/onekvm/config.json";
#[allow(dead_code)]
pub const SYSTEMD_RUN: &str = "/bin/systemd-run";
pub const MODPROBE: &str = "/usr/sbin/modprobe";
pub const DEPMOD: &str = "/usr/sbin/depmod";
pub const OPKG: &str = "/usr/bin/opkg";
pub const TUN_DEVICE: &str = "/dev/net/tun";
pub const HOST_BINARY: &str = "/usr/bin/onekvm-extension-host";
pub const EXTENSIONS_SLICE: &str = "system-onekvm-extensions.slice";
pub const TRANSIENT_ROOT: &str = "/run/systemd/transient";
pub const RUNTIME_UNIT_ROOT: &str = "/run/systemd/system";
pub const SETTINGS_OUTPUT: &str = "settings-output.json";
pub const DIRECTORIES_OUTPUT: &str = "directories-output.json";
pub const MAX_LOCAL_IPK: u64 = 32 << 20;

pub fn state_path(id: &str) -> PathBuf {
    Path::new(CONFIG_ROOT).join(format!("{id}.json"))
}

pub fn app_root(id: &str, version: &str) -> PathBuf {
    Path::new(APPS_ROOT).join(id).join(version)
}

pub fn helper_unit(id: &str) -> String {
    format!("onekvm-extension-{id}@helper.service")
}

pub fn helper_address(id: &str) -> PathBuf {
    Path::new(RUNTIME_ROOT)
        .join(id)
        .join("sockets")
        .join("helper.bus")
}

pub fn unit_name(id: &str) -> String {
    format!("onekvm-extension-{id}.service")
}

pub fn socket_unit(id: &str) -> String {
    format!("onekvm-extension-{id}.socket")
}

pub fn valid_extension_service_unit(id: &str, unit: &str) -> bool {
    if !valid_id(id) || !unit.ends_with(".service") {
        return false;
    }
    let name = unit.strip_suffix(".service").unwrap_or_default();
    let direct = format!("onekvm-{id}");
    let extension = format!("onekvm-extension-{id}");
    name == direct
        || name.starts_with(&format!("{direct}-"))
        || name == extension
        || name.starts_with(&format!("{extension}-"))
}

pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some('a'..='z' | '0'..='9'))
        && id.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn valid_version(version: &str) -> bool {
    let mut chars = version.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && version.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

pub fn valid_method_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn valid_package_name(name: &str) -> bool {
    name.starts_with("onekvm-extension-") && valid_id(name.trim_start_matches("onekvm-extension-"))
}

/// User-chosen key for `/etc/opkg/onekvm-ext-{key}.conf`.
pub fn valid_feed_source_name(name: &str) -> bool {
    valid_id(name)
}

pub fn valid_feed_source_key(key: &str) -> bool {
    valid_id(key)
}

/// System feed plus marketplace drop-ins (`onekvm-ext-*.conf`), sorted.
pub fn feed_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let system = Path::new(FEED_CONFIG);
    if system.is_file() {
        paths.push(system.to_path_buf());
    }
    let Ok(entries) = fs::read_dir(OPKG_CONF_DIR) else {
        return paths;
    };
    let mut custom = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with(FEED_CUSTOM_PREFIX) || !name.ends_with(FEED_CUSTOM_SUFFIX) {
            continue;
        }
        let stem = &name[FEED_CUSTOM_PREFIX.len()..name.len() - FEED_CUSTOM_SUFFIX.len()];
        if !valid_feed_source_name(stem) {
            continue;
        }
        let path = entry.path();
        if path.is_file() {
            custom.push(path);
        }
    }
    custom.sort();
    paths.extend(custom);
    paths
}

pub fn disabled_feed_path(path: &Path) -> PathBuf {
    match path.file_name() {
        Some(name) => {
            let mut name = name.to_os_string();
            name.push(FEED_DISABLED_SUFFIX);
            path.with_file_name(name)
        }
        None => {
            let mut out = path.as_os_str().to_os_string();
            out.push(FEED_DISABLED_SUFFIX);
            PathBuf::from(out)
        }
    }
}

pub fn valid_directory_kind(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && value.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn valid_upload_name(name: &str) -> bool {
    name.len() == 39
        && name.ends_with(".upload")
        && name.as_bytes()[..32]
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn safe_relative(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && name != "."
        && name != ".."
        && !name.starts_with("../")
        && Path::new(name)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::disabled_feed_path;
    use std::path::Path;

    #[test]
    fn helper_units_cannot_collide_with_extension_service_names() {
        assert_eq!(
            super::helper_unit("cloud"),
            "onekvm-extension-cloud@helper.service"
        );
        assert_ne!(
            super::helper_unit("cloud"),
            super::unit_name("cloud-helper")
        );
    }

    #[test]
    fn disabled_feed_path_does_not_end_in_conf() {
        let disabled = disabled_feed_path(Path::new("/etc/opkg/onekvm-feed.conf"));
        assert_eq!(disabled, Path::new("/etc/opkg/onekvm-feed.conf.disabled"));
        assert_eq!(
            disabled.extension().and_then(|value| value.to_str()),
            Some("disabled")
        );
    }
}
