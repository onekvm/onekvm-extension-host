mod catalog;
mod cli;
mod control;
mod dbus_client;
mod feed;
mod ffi;
mod helper;
mod helper_client;
mod host;
mod install;
mod ipk;
mod ipks;
mod lifecycle;
mod manifest;
mod memory;
mod paths;
mod ports;
mod sandbox;
mod settings;
mod state;
mod sys;
mod system_plugin;

fn main() {
    let mut arguments = std::env::args();
    let argv0 = arguments
        .next()
        .unwrap_or_else(|| "onekvm-extension-host".into());
    let rest: Vec<String> = arguments.collect();
    let result = if rest.first().map(String::as_str) == Some("--plugin-helper") {
        helper::run(rest.into_iter().skip(1).collect())
    } else if rest.is_empty() && is_host_binary(&argv0) {
        run_host()
    } else {
        cli::run(&argv0, &rest)
    };
    if let Err(err) = result {
        let name = std::path::Path::new(&argv0)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("onekvm-extension-host");
        eprintln!("{name}: {err}");
        std::process::exit(1);
    }
}

fn is_host_binary(argv0: &str) -> bool {
    std::path::Path::new(argv0)
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name == "onekvm-extension-host")
}

fn run_host() -> Result<(), Box<dyn std::error::Error>> {
    host::repair_permissions()?;
    let host = Box::into_raw(Box::new(host::Host::new()));
    let status = unsafe { ffi::onekvm_host_serve(host.cast()) };
    unsafe {
        drop(Box::from_raw(host));
    }
    if status < 0 {
        return Err(format!("sd-bus host failed ({status})").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::paths;

    #[test]
    fn id_pattern_accepts_oled() {
        assert!(paths::valid_id("nanokvm-oled"));
        assert!(!paths::valid_id("Bad"));
    }
}
