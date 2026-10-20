use std::io::{self, Read, Write};

use crate::catalog;
use crate::contributions;
use crate::feed;
use crate::install;
use crate::lifecycle;
use crate::paths;
use crate::sys;

const USAGE: &str = "\
usage: onekvm-extension <command> [arguments]

commands:
  list
  catalog
  contributions <point>
  detail <id>
  status <id>
  health <id>
  validate <id> [version]
  activate <id> [version]
  enable <id>
  disable <id>
  start <id>
  stop <id>
  restart <id>
  service-action <id> <service> <start|stop|restart|enable|disable>
  configure <id>  (JSON request on stdin)
  refresh
  install <onekvm-extension-name>
  install-local  (IPK or IPKS on stdin)
  inspect-local-path <staged-path>
  install-local-path <staged-path>
  remove <onekvm-extension-name>
  purge <id>
  restore
  load-network-module <module>
  invoke <id> <method>  (JSON request on stdin)";

pub fn run(argv0: &str, args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err(usage(argv0).into());
    }
    if args[0].starts_with("update-") {
        let value = sys::with_lock(|| crate::update::command(args, &mut io::stdin()))?;
        return Ok(write_json(&value)?);
    }
    const COMMANDS: &[&str] = &[
        "list",
        "catalog",
        "contributions",
        "detail",
        "status",
        "health",
        "validate",
        "activate",
        "enable",
        "disable",
        "start",
        "stop",
        "restart",
        "service-action",
        "configure",
        "install",
        "refresh",
        "install-local",
        "inspect-local-path",
        "install-local-path",
        "remove",
        "purge",
        "restore",
        "load-network-module",
        "invoke",
    ];
    if !COMMANDS.contains(&args[0].as_str()) {
        return Err(format!("unknown command {:?}\n{}", args[0], usage(argv0)).into());
    }
    sys::with_lock(|| { crate::transaction::recover_all(true)?; lifecycle::migrate_legacy_layout() })
        .map_err(|err| format!("migrate extension configuration: {err}"))?;
    match args[0].as_str() {
        "load-network-module" => {
            require_len(args, 2, "load-network-module requires <module>")?;
            crate::dbus_client::load_network_module(&args[1])?;
        }
        "list" => {
            require_len(args, 1, "list takes no arguments")?;
            print!("{}", lifecycle::list()?);
        }
        "catalog" => {
            require_len(args, 1, "catalog takes no arguments")?;
            write_json(&catalog::catalog()?)?;
        }
        "contributions" => {
            require_len(args, 2, "contributions requires <point>")?;
            write_json(&contributions::list(&args[1])?)?;
        }
        "detail" => {
            write_json(&catalog::detail(&one_id(args)?)?)?;
        }
        "health" => { crate::lifecycle::check_started(&one_id(args)?)?; }
        "status" => {
            write_json(&lifecycle::status(&one_id(args)?)?)?;
        }
        "validate" => {
            if args.len() < 2 || args.len() > 3 {
                return Err("validate requires <id> [version]".into());
            }
            print!("{}", lifecycle::validate(&args[1], optional(args, 2))?);
        }
        "activate" => {
            if args.len() < 2 || args.len() > 3 {
                return Err("activate requires <id> [version]".into());
            }
            lifecycle::activate(&args[1], optional(args, 2))?;
        }
        "enable" => lifecycle::enable(&one_id(args)?)?,
        "disable" => lifecycle::disable(&one_id(args)?)?,
        "start" => lifecycle::start(&one_id(args)?)?,
        "stop" => lifecycle::stop(&one_id(args)?)?,
        "restart" => lifecycle::restart(&one_id(args)?)?,
        "service-action" => {
            if args.len() != 4 {
                return Err(
                    "service-action requires <id> <service> <start|stop|restart|enable|disable>"
                        .into(),
                );
            }
            lifecycle::service_action(&args[1], &args[2], &args[3])?;
        }
        "configure" => lifecycle::configure(&one_id(args)?, &mut io::stdin())?,
        "install" => install::install_package(&one_id(args)?)?,
        "refresh" => {
            require_len(args, 1, "refresh takes no arguments")?;
            feed::refresh()?;
        }
        "install-local" => {
            require_len(args, 1, "install-local takes no arguments")?;
            install::install_local(&mut io::stdin())?;
        }
        "inspect-local-path" => {
            write_json(&install::inspect_local_path(&one_id(args)?)?)?;
        }
        "install-local-path" => install::install_local_path(&one_id(args)?)?,
        "remove" => install::remove_package(&one_id(args)?)?,
        "purge" => install::purge(&one_id(args)?)?,
        "restore" => {
            require_len(args, 1, "restore takes no arguments")?;
            lifecycle::restore()?;
        }
        "invoke" => {
            if args.len() != 3 {
                return Err("invoke requires <id> <method>".into());
            }
            if !paths::valid_id(&args[1]) {
                return Err(format!("invalid extension id {:?}", args[1]).into());
            }
            if !paths::valid_method_name(&args[2]) {
                return Err(format!("invalid extension method {:?}", args[2]).into());
            }
            let mut request = Vec::new();
            io::stdin().take((1 << 20) + 1).read_to_end(&mut request)?;
            if request.len() > 1 << 20 {
                return Err("public method request exceeds 1 MiB".into());
            }
            if request.is_empty() {
                request = b"null".to_vec();
            }
            if serde_json::from_slice::<serde_json::Value>(&request).is_err() {
                return Err("public method request is not valid JSON".into());
            }
            println!("{}", lifecycle::invoke(&args[1], &args[2], &request)?);
        }
        other => return Err(format!("unknown command {other:?}\n{}", usage(argv0)).into()),
    }
    Ok(())
}

fn usage(argv0: &str) -> String {
    let name = std::path::Path::new(argv0)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("onekvm-extension");
    USAGE.replace("onekvm-extension", name)
}

fn require_len(args: &[String], len: usize, message: &str) -> Result<(), String> {
    if args.len() != len {
        Err(message.into())
    } else {
        Ok(())
    }
}

fn one_id(args: &[String]) -> Result<String, String> {
    if args.len() != 2 {
        return Err(format!("{} requires one argument", args[0]));
    }
    Ok(args[1].clone())
}

fn optional(args: &[String], index: usize) -> &str {
    args.get(index).map(String::as_str).unwrap_or_default()
}

fn write_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let mut stdout = io::stdout();
    serde_json::to_writer(&mut stdout, value).map_err(|e| e.to_string())?;
    stdout.write_all(b"\n").map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn unknown_command_mentions_usage() {
        let err = run("onekvm-extension-host", &["nope".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown command"));
    }

    #[test]
    fn cli_name_without_args_prints_usage() {
        let err = run("onekvm-extension", &[]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("usage: onekvm-extension"), "{text}");
        assert!(!text.contains("onekvm-extension-host"));
    }
}
