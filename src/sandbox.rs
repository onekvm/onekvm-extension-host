use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::manifest::Installed;
use crate::paths;
use crate::sys;

pub fn sandbox_arguments(extension: &Installed) -> Vec<String> {
    let id = &extension.manifest.id;
    let permissions: std::collections::BTreeSet<_> = extension
        .manifest
        .permissions
        .iter()
        .map(|value| value.as_str())
        .collect();
    if permissions.contains("host-unrestricted") {
        return unrestricted_host_arguments();
    }
    let (runtime_user, runtime_group) = sandbox_identity(&permissions);
    let mut arguments = vec![
        "--property=Type=exec".into(),
        format!("--property=User={runtime_user}"),
        format!("--property=Group={runtime_group}"),
        "--property=NoNewPrivileges=yes".into(),
        "--property=ProtectSystem=strict".into(),
        "--property=ProtectHome=yes".into(),
        "--property=PrivateTmp=yes".into(),
        "--property=ProtectKernelTunables=yes".into(),
        "--property=ProtectKernelModules=yes".into(),
        "--property=ProtectKernelLogs=yes".into(),
        "--property=ProtectControlGroups=yes".into(),
        "--property=ProtectHostname=yes".into(),
        "--property=RestrictSUIDSGID=yes".into(),
        "--property=LockPersonality=yes".into(),
        "--property=RestrictRealtime=yes".into(),
        "--property=SystemCallArchitectures=native".into(),
        "--property=UMask=0027".into(),
        format!("--property=ReadOnlyPaths={}", extension.root.display()),
        format!(
            "--property=ReadOnlyPaths={} {}",
            paths::APP_DATA_ROOT,
            paths::RUNTIME_ROOT
        ),
        format!(
            "--property=ReadWritePaths={}",
            Path::new(paths::APP_DATA_ROOT).join(id).display()
        ),
        format!(
            "--property=ReadWritePaths={}",
            Path::new(paths::RUNTIME_ROOT)
                .join(id)
                .join("sockets")
                .display()
        ),
    ];
    if id != "nanokvm-oled" {
        arguments.push("--property=ProtectClock=yes".into());
    }
    if let Some(families) = network_address_families(&permissions) {
        arguments.push(format!("--property=RestrictAddressFamilies={families}"));
    } else {
        arguments.push("--property=PrivateNetwork=yes".into());
        arguments.push("--property=RestrictAddressFamilies=AF_UNIX".into());
    }
    let mut devices = Vec::new();
    let mut supplementary_groups = Vec::new();
    if permissions.contains("device-video") {
        devices.extend(glob_paths("/dev/video*"));
    }
    if permissions.contains("device-kvm") && Path::new("/dev/kvm").exists() {
        devices.push("/dev/kvm".into());
    }
    if permissions.contains("device-crypto") && Path::new("/dev/onekvm-crypto-offload").exists() {
        devices.push("/dev/onekvm-crypto-offload".into());
        supplementary_groups.push("onekvm-crypto");
    }
    if permissions.contains("device-hid") {
        devices.extend(glob_paths("/dev/hidg*"));
    }
    if permissions.contains("device-i2c") {
        devices.push("char-i2c".into());
        devices.push("/dev/onekvm-oled".into());
    }
    if permissions.contains("device-fb") {
        devices.extend(glob_paths("/dev/fb*"));
        supplementary_groups.push("video");
        for path in ["/sys/class/backlight", "/sys/class/graphics"] {
            if Path::new(path).exists() {
                arguments.push(format!("--property=ReadWritePaths={path}"));
            }
        }
    }
    if permissions.contains("gpio") {
        devices.extend(glob_paths("/dev/gpiochip*"));
        devices.extend(glob_paths("/dev/input/event*"));
    }
    if permissions.contains("network-admin") {
        devices.push("/dev/net/tun".into());
    }
    if !supplementary_groups.is_empty() {
        arguments.push(format!(
            "--property=SupplementaryGroups={}",
            supplementary_groups.join(" ")
        ));
    }
    if devices.is_empty() {
        arguments.push("--property=PrivateDevices=yes".into());
    } else {
        devices.sort();
        arguments.push("--property=PrivateDevices=yes".into());
        arguments.push("--property=DevicePolicy=closed".into());
        for device in devices {
            arguments.push(format!("--property=DeviceAllow={device} rw"));
            if device.starts_with("/dev/") {
                arguments.push(format!("--property=BindPaths={device}"));
            }
        }
    }
    arguments.extend(capability_arguments(&permissions));
    if permissions.contains("opkg-admin") {
        // Marketplace writes onekvm-ext-{name}.conf. Sticky bit on the directory
        // keeps root-owned opkg.conf / onekvm-feed.conf from being replaced.
        // Browse/install UI also needs read access to signed opkg lists and the
        // installed extension tree; privileged install/remove is handled by the
        // host (see apply_command_method), not inside this sandbox.
        arguments.push(format!(
            "--property=ReadWritePaths={}",
            paths::OPKG_CONF_DIR
        ));
        arguments.push(format!("--property=ReadOnlyPaths={}", paths::OPKG_LISTS));
        arguments.push(format!("--property=ReadOnlyPaths={}", paths::APPS_ROOT));
    }
    let limits = &extension.manifest.resource_limits;
    if !limits.memory_max.is_empty() {
        arguments.push(format!("--property=MemoryMax={}", limits.memory_max));
    }
    if !limits.cpu_quota.is_empty() {
        arguments.push(format!("--property=CPUQuota={}", limits.cpu_quota));
    }
    if limits.tasks_max > 0 {
        arguments.push(format!("--property=TasksMax={}", limits.tasks_max));
    }
    if limits.limit_nofile > 0 {
        arguments.push(format!("--property=LimitNOFILE={}", limits.limit_nofile));
    }
    arguments
}

fn sandbox_identity(
    permissions: &std::collections::BTreeSet<&str>,
) -> (&'static str, &'static str) {
    if permissions.contains("host-root") || permissions.contains("host-unrestricted") {
        ("root", "onekvm-extension")
    } else {
        ("onekvm-extension", "onekvm-extension")
    }
}

fn unrestricted_host_arguments() -> Vec<String> {
    // This opt-in profile deliberately adds no filesystem, device, network,
    // namespace, capability, syscall, or resource restrictions. It is for a
    // host-administration surface such as the terminal, not a normal plugin.
    vec![
        "--property=Type=exec".into(),
        "--property=User=root".into(),
        "--property=Group=root".into(),
    ]
}

fn capability_arguments(permissions: &std::collections::BTreeSet<&str>) -> Vec<String> {
    let host_root = permissions.contains("host-root") || permissions.contains("host-unrestricted");
    let network_admin = permissions.contains("network-admin");
    let mut arguments = Vec::new();
    let mut ambient = Vec::new();

    if host_root {
        // RestrictSUIDSGID removes CAP_SETUID from the helper's effective set.
        // Explicit host-root extensions may need to drop child processes back
        // to the unprivileged extension identity, so retain only the identity
        // transition capabilities. RestrictSUIDSGID still blocks SUID/SGID
        // file creation, and the capabilities disappear after setuid().
        ambient.extend(["CAP_SETGID", "CAP_SETUID"]);
    }
    if network_admin {
        // Non-root extensions receive only their declared network capabilities.
        // host-root already has the full bounding set, so narrowing it here
        // would accidentally remove the identity transition capabilities.
        if !host_root {
            arguments.push("--property=CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW".into());
        }
        ambient.extend(["CAP_NET_ADMIN", "CAP_NET_RAW"]);
    }
    if !ambient.is_empty() {
        arguments.push(format!(
            "--property=AmbientCapabilities={}",
            ambient.join(" ")
        ));
    }
    arguments
}

fn network_address_families(
    permissions: &std::collections::BTreeSet<&str>,
) -> Option<&'static str> {
    if !permissions.contains("network") {
        return None;
    }
    if permissions.contains("network-info") || permissions.contains("network-admin") {
        return Some("AF_UNIX AF_INET AF_INET6 AF_NETLINK");
    }
    Some("AF_UNIX AF_INET AF_INET6")
}

fn glob_paths(pattern: &str) -> Vec<String> {
    let Ok(entries) = glob_simple(pattern) else {
        return Vec::new();
    };
    entries
}

fn glob_simple(pattern: &str) -> Result<Vec<String>, String> {
    if let Some(dir) = pattern.rfind('/') {
        let (directory, name) = pattern.split_at(dir);
        let name = &name[1..];
        if !name.ends_with('*') {
            return Ok(if Path::new(pattern).exists() {
                vec![pattern.to_owned()]
            } else {
                Vec::new()
            });
        }
        let prefix = name.trim_end_matches('*');
        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let text = file_name.to_string_lossy();
                if text.starts_with(prefix) {
                    matches.push(entry.path().to_string_lossy().into_owned());
                }
            }
        }
        matches.sort();
        Ok(matches)
    } else {
        Ok(Vec::new())
    }
}

pub fn helper_sandbox_arguments(extension: &Installed) -> Vec<String> {
    let mut arguments = sandbox_arguments(extension);
    for argument in &mut arguments {
        if argument == "--property=ProtectControlGroups=yes" {
            *argument = "--property=ProtectControlGroups=no".into();
        }
    }
    arguments.push("--property=Delegate=yes".into());
    arguments
}

pub fn load_kernel_modules(extension: &Installed) -> Result<(), String> {
    if extension.manifest.is_environment() {
        return Ok(());
    }
    for module in &extension.manifest.kernel_modules {
        sys::run(paths::MODPROBE, &[module.clone()])
            .map_err(|err| format!("load kernel module {module}: {err}"))?;
    }
    if extension
        .manifest
        .permissions
        .iter()
        .any(|permission| permission == "network-admin")
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(paths::TUN_DEVICE, std::fs::Permissions::from_mode(0o666))
            .map_err(|e| format!("prepare TUN device: {e}"))?;
    }
    if extension
        .manifest
        .permissions
        .iter()
        .any(|permission| permission == "opkg-admin")
    {
        prepare_opkg_feed_config()?;
    }
    Ok(())
}

fn prepare_opkg_feed_config() -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let dir = Path::new(paths::OPKG_CONF_DIR);
    std::fs::create_dir_all(dir).map_err(|e| format!("prepare opkg config dir: {e}"))?;
    let (_, gid) = crate::sys::runtime_ids()?;
    let dir_c = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| "invalid opkg config dir path".to_string())?;
    // root:onekvm-extension + sticky: create onekvm-ext-*.conf, cannot unlink root files.
    if unsafe { libc::chown(dir_c.as_ptr(), 0, gid) } != 0 {
        return Err(format!(
            "chown opkg config dir: {}",
            std::io::Error::last_os_error()
        ));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1775))
        .map_err(|e| format!("chmod opkg config dir: {e}"))?;

    let path = Path::new(paths::FEED_CONFIG);
    if !path.exists() && !paths::disabled_feed_path(path).exists() {
        std::fs::write(
            path,
            b"# OneKVM system/default extension feed configuration\n",
        )
        .map_err(|e| format!("create opkg feed config: {e}"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("chmod opkg feed config: {e}"))?;
    }
    Ok(())
}

extern "C" {
    fn onekvm_start_transient_service(
        unit: *const c_char,
        slice: *const c_char,
        working_directory: *const c_char,
        properties: *mut *mut c_char,
        nproperties: usize,
        environment: *mut *mut c_char,
        nenv: usize,
        command: *mut *mut c_char,
        ncommand: usize,
        socket_unit: *const c_char,
        listen_address: *const c_char,
    ) -> c_int;
    fn onekvm_systemd_unit_status(unit: *const c_char) -> c_int;
    fn onekvm_systemd_unit_has_dependency(
        unit: *const c_char,
        property: *const c_char,
        dependency: *const c_char,
    ) -> c_int;
    fn onekvm_systemd_unit_action(action: *const c_char, unit: *const c_char) -> c_int;
}

struct CStrings {
    owned: Vec<CString>,
    pointers: Vec<*mut c_char>,
}

impl CStrings {
    fn new(values: &[String]) -> Result<Self, String> {
        let mut owned = Vec::with_capacity(values.len());
        let mut pointers = Vec::with_capacity(values.len());
        for value in values {
            let value =
                CString::new(value.as_str()).map_err(|_| "NUL in transient unit argument")?;
            pointers.push(value.as_ptr() as *mut c_char);
            owned.push(value);
        }
        Ok(Self { owned, pointers })
    }
}

// This accepts the small systemd-run-compatible argument vocabulary built by
// host.rs, but sends StartTransientUnit directly. Keeping the parsing here
// makes the service definitions easy to audit while avoiding a process spawn
// and a second command-line parser on every extension start.
pub fn systemd_run(args: &[String]) -> Result<(), String> {
    systemd_run_with_socket(args, None)
}

pub fn systemd_run_with_socket(
    args: &[String],
    socket: Option<(&str, &str)>,
) -> Result<(), String> {
    let mut unit = None;
    let mut slice = None;
    let mut working_directory = None;
    let mut properties = Vec::new();
    let mut environment = Vec::new();
    let mut command = Vec::new();
    let mut after_separator = false;

    for argument in args {
        if after_separator {
            command.push(argument.clone());
        } else if argument == "--" {
            after_separator = true;
        } else if argument == "--quiet" {
            continue;
        } else if argument == "--collect" {
            properties.push("CollectMode=inactive-or-failed".into());
        } else if let Some(value) = argument.strip_prefix("--unit=") {
            unit = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--slice=") {
            slice = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--property=") {
            properties.push(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--working-directory=") {
            working_directory = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--setenv=") {
            environment.push(value.to_owned());
        } else {
            return Err(format!("unsupported transient unit argument {argument:?}"));
        }
    }

    let unit = unit.ok_or_else(|| "transient unit name is missing".to_string())?;
    if command.is_empty() {
        return Err("transient unit command is missing".into());
    }
    let unit = CString::new(unit).map_err(|_| "NUL in transient unit name")?;
    let slice = slice
        .map(CString::new)
        .transpose()
        .map_err(|_| "NUL in transient unit slice")?;
    let working_directory = working_directory
        .map(CString::new)
        .transpose()
        .map_err(|_| "NUL in transient working directory")?;
    let mut properties = CStrings::new(&properties)?;
    let mut environment = CStrings::new(&environment)?;
    let mut command = CStrings::new(&command)?;
    let socket = socket
        .map(|(unit, address)| {
            Ok::<_, String>((
                CString::new(unit).map_err(|_| "NUL in socket unit name")?,
                CString::new(address).map_err(|_| "NUL in socket address")?,
            ))
        })
        .transpose()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        let status = unsafe {
            onekvm_start_transient_service(
                unit.as_ptr(),
                slice
                    .as_ref()
                    .map_or(std::ptr::null(), |value| value.as_ptr()),
                working_directory
                    .as_ref()
                    .map_or(std::ptr::null(), |value| value.as_ptr()),
                properties.pointers.as_mut_ptr(),
                properties.owned.len(),
                environment.pointers.as_mut_ptr(),
                environment.owned.len(),
                command.pointers.as_mut_ptr(),
                command.owned.len(),
                socket
                    .as_ref()
                    .map_or(std::ptr::null(), |(unit, _)| unit.as_ptr()),
                socket
                    .as_ref()
                    .map_or(std::ptr::null(), |(_, address)| address.as_ptr()),
            )
        };
        if status != -libc::EEXIST || Instant::now() >= deadline {
            break status;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    if status < 0 {
        Err(format!("StartTransientUnit failed ({status})"))
    } else {
        Ok(())
    }
}

const UNIT_STATUS_ACTIVE: c_int = 1 << 0;
const UNIT_STATUS_FAILED: c_int = 1 << 1;
const UNIT_STATUS_ENABLED: c_int = 1 << 2;

fn unit_status(unit: &str) -> Result<c_int, String> {
    let unit = CString::new(unit).map_err(|_| "NUL in systemd unit name".to_string())?;
    let status = unsafe { onekvm_systemd_unit_status(unit.as_ptr()) };
    if status < 0 {
        Err(format!("query systemd unit failed ({status})"))
    } else {
        Ok(status)
    }
}

pub fn unit_action(action: &str, unit: &str) -> Result<(), String> {
    let action_name = action;
    let unit_name = unit;
    let action = CString::new(action).map_err(|_| "NUL in systemd action".to_string())?;
    let unit = CString::new(unit).map_err(|_| "NUL in systemd unit name".to_string())?;
    let status = unsafe { onekvm_systemd_unit_action(action.as_ptr(), unit.as_ptr()) };
    if status < 0 {
        Err(format!(
            "systemd {action_name} {unit_name} failed ({status})"
        ))
    } else {
        Ok(())
    }
}

pub fn unit_has_dependency(unit: &str, property: &str, dependency: &str) -> bool {
    let Ok(unit) = CString::new(unit) else {
        return false;
    };
    let Ok(property) = CString::new(property) else {
        return false;
    };
    let Ok(dependency) = CString::new(dependency) else {
        return false;
    };
    unsafe {
        onekvm_systemd_unit_has_dependency(unit.as_ptr(), property.as_ptr(), dependency.as_ptr())
            > 0
    }
}

pub fn is_active(unit: &str) -> bool {
    unit_status(unit)
        .map(|status| status & UNIT_STATUS_ACTIVE != 0)
        .unwrap_or(false)
}

pub fn is_failed(unit: &str) -> bool {
    unit_status(unit)
        .map(|status| status & UNIT_STATUS_FAILED != 0)
        .unwrap_or(false)
}

pub fn is_enabled(unit: &str) -> bool {
    unit_status(unit)
        .map(|status| status & UNIT_STATUS_ENABLED != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        capability_arguments, network_address_families, sandbox_identity,
        unrestricted_host_arguments,
    };
    use std::collections::BTreeSet;

    fn permissions(values: &[&'static str]) -> BTreeSet<&'static str> {
        values.iter().copied().collect()
    }

    #[test]
    fn network_info_allows_netlink_without_network_admin() {
        assert_eq!(
            network_address_families(&permissions(&["network", "network-info"])),
            Some("AF_UNIX AF_INET AF_INET6 AF_NETLINK")
        );
        assert_eq!(
            network_address_families(&permissions(&["network"])),
            Some("AF_UNIX AF_INET AF_INET6")
        );
        assert_eq!(
            network_address_families(&permissions(&["network-info"])),
            None
        );
    }

    #[test]
    fn host_root_selects_root_identity_without_changing_other_permissions() {
        assert_eq!(
            sandbox_identity(&permissions(&["network", "host-root"])),
            ("root", "onekvm-extension")
        );
        assert_eq!(
            sandbox_identity(&permissions(&["network"])),
            ("onekvm-extension", "onekvm-extension")
        );
        assert_eq!(
            sandbox_identity(&permissions(&["host-unrestricted"])),
            ("root", "onekvm-extension")
        );
    }

    #[test]
    fn unrestricted_host_profile_adds_no_sandbox_or_resource_properties() {
        assert_eq!(
            unrestricted_host_arguments(),
            vec![
                "--property=Type=exec",
                "--property=User=root",
                "--property=Group=root",
            ]
        );
    }

    #[test]
    fn host_root_retains_only_the_capabilities_needed_to_drop_identity() {
        assert_eq!(
            capability_arguments(&permissions(&["host-root"])),
            vec!["--property=AmbientCapabilities=CAP_SETGID CAP_SETUID"]
        );
        assert_eq!(
            capability_arguments(&permissions(&["network-admin"])),
            vec![
                "--property=CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW",
                "--property=AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW",
            ]
        );
        assert_eq!(
            capability_arguments(&permissions(&["host-root", "network-admin"])),
            vec!["--property=AmbientCapabilities=CAP_SETGID CAP_SETUID CAP_NET_ADMIN CAP_NET_RAW"]
        );
    }
}
