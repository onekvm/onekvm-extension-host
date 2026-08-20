use std::path::{Path, PathBuf};

pub const APPS_ROOT: &str = "/data/onekvm/apps";
pub const APP_DATA_ROOT: &str = "/data/onekvm/app-data";
pub const CONFIG_ROOT: &str = "/etc/onekvm/extensions";
pub const RUNTIME_ROOT: &str = "/run/onekvm/extensions";
pub const AUTH_ROOT: &str = "/run/onekvm/extension-auth";
pub const LOCK_PATH: &str = "/run/onekvm/extension-manager.lock";
pub const GENERATION_PATH: &str = "/run/onekvm/extension-host-generation";
pub const MACHINE_PROFILE: &str = "/usr/share/onekvm/machine.json";
pub const MACHINE_HARDWARE: &str = "/run/onekvm/hardware.json";
pub const SYSTEMD_RUN: &str = "/bin/systemd-run";
pub const SYSTEMCTL: &str = "/bin/systemctl";
pub const MODPROBE: &str = "/usr/sbin/modprobe";
pub const TUN_DEVICE: &str = "/dev/net/tun";
pub const HOST_BINARY: &str = "/usr/bin/onekvm-extension-host";
pub const EXTENSIONS_SLICE: &str = "system-onekvm-extensions.slice";
pub const TRANSIENT_ROOT: &str = "/run/systemd/transient";
pub const RUNTIME_UNIT_ROOT: &str = "/run/systemd/system";
pub const SETTINGS_OUTPUT: &str = "settings-output.json";

pub fn state_path(id: &str) -> PathBuf {
    Path::new(CONFIG_ROOT).join(format!("{id}.json"))
}

pub fn app_root(id: &str, version: &str) -> PathBuf {
    Path::new(APPS_ROOT).join(id).join(version)
}

pub fn helper_unit(id: &str) -> String {
    format!("onekvm-extension-{id}-helper.service")
}

pub fn helper_address(id: &str) -> PathBuf {
    Path::new(RUNTIME_ROOT).join(id).join("sockets").join("helper.bus")
}

pub fn unit_name(id: &str) -> String {
    format!("onekvm-extension-{id}.service")
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
