mod ffi;
mod helper;
mod helper_client;
mod host;
mod manifest;
mod paths;
mod sandbox;
mod settings;
mod state;
mod sys;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let result = if arguments.next().as_deref() == Some("--plugin-helper") {
        helper::run(arguments.collect())
    } else {
        run_host()
    };
    if let Err(err) = result {
        eprintln!("onekvm-extension-host: {err}");
        std::process::exit(1);
    }
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
