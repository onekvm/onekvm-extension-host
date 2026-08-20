use std::path::Path;

use crate::manifest::Installed;
use crate::paths;
use crate::sys;

pub fn sandbox_arguments(extension: &Installed) -> Vec<String> {
    let id = &extension.manifest.id;
    let mut arguments = vec![
        "--property=Type=exec".into(),
        "--property=User=onekvm-extension".into(),
        "--property=Group=onekvm-extension".into(),
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
    let permissions: std::collections::BTreeSet<_> = extension
        .manifest
        .permissions
        .iter()
        .map(|value| value.as_str())
        .collect();
    if permissions.contains("network") {
        let mut families = String::from("AF_UNIX AF_INET AF_INET6");
        if permissions.contains("network-admin") {
            families.push_str(" AF_NETLINK");
        }
        arguments.push(format!("--property=RestrictAddressFamilies={families}"));
    } else {
        arguments.push("--property=PrivateNetwork=yes".into());
        arguments.push("--property=RestrictAddressFamilies=AF_UNIX".into());
    }
    let mut devices = Vec::new();
    if permissions.contains("device-video") {
        devices.extend(glob_paths("/dev/video*"));
    }
    if permissions.contains("device-hid") {
        devices.extend(glob_paths("/dev/hidg*"));
    }
    if permissions.contains("device-i2c") {
        devices.push("char-i2c".into());
        devices.push("/dev/onekvm-oled".into());
    }
    if permissions.contains("gpio") {
        devices.extend(glob_paths("/dev/gpiochip*"));
    }
    if permissions.contains("network-admin") {
        devices.push("/dev/net/tun".into());
    }
    if devices.is_empty() {
        arguments.push("--property=PrivateDevices=yes".into());
    } else {
        devices.sort();
        arguments.push("--property=PrivateDevices=no".into());
        arguments.push("--property=DevicePolicy=closed".into());
        for device in devices {
            arguments.push(format!("--property=DeviceAllow={device} rw"));
        }
    }
    if permissions.contains("network-admin") {
        arguments.push("--property=CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW".into());
        arguments.push("--property=AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW".into());
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
    Ok(())
}

pub fn systemd_run(args: &[String]) -> Result<(), String> {
    sys::run(paths::SYSTEMD_RUN, args)
}

pub fn systemctl(args: &[&str]) -> Result<(), String> {
    let owned: Vec<String> = args.iter().map(|value| (*value).to_owned()).collect();
    sys::run(paths::SYSTEMCTL, &owned)
}

pub fn is_active(unit: &str) -> bool {
    sys::quiet(paths::SYSTEMCTL, &["is-active", "--quiet", unit])
}
