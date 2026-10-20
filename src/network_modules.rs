//! Privileged module loading requested by a configured network interface.
//! The Core service intentionally lacks CAP_SYS_MODULE. Installing or enabling
//! an environment must never invoke this path.
use std::path::Path;

fn allowed(module: &str) -> bool {
    matches!(
        module,
        "wireguard"
            | "ip_gre"
            | "ipip"
            | "sit"
            | "vxlan"
            | "geneve"
            | "fou"
            | "bareudp"
            | "tun"
            | "ppp_generic"
            | "ppp_async"
            | "pppoe"
            | "pptp"
            | "l2tp_ppp"
    )
}

pub fn load(module: &str) -> Result<(), String> {
    if !allowed(module) {
        return Err(format!("unsupported network kernel module {module:?}"));
    }
    crate::sys::with_lock(|| {
        if Path::new("/sys/module").join(module).exists() {
            return Ok(());
        }
        crate::sys::run(crate::paths::MODPROBE, &[module.into()])
            .map_err(|err| format!("load network kernel module {module}: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_network_module_names_are_accepted() {
        for name in ["wireguard", "ip_gre", "tun", "l2tp_ppp"] {
            assert!(allowed(name));
        }
        for name in [
            "",
            "../ip_gre",
            "-r",
            "vxlan debug=1",
            "usb_storage",
            "rtnl-link-gre",
        ] {
            assert!(load(name)
                .unwrap_err()
                .contains("unsupported network kernel module"));
        }
    }
}
