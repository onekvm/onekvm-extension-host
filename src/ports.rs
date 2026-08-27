use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use crate::paths;
use crate::state::read_state;

pub fn validate_extension_port(id: &str, settings: &Value, allow_current: bool) -> Result<(), String> {
    let Some(port) = setting_port(settings) else {
        return Ok(());
    };
    let web_port = configured_web_port()?;
    if port == web_port {
        return Err(format!("port {port} is configured for the OneKVM Web service"));
    }
    if let Some(owner) = configured_owner(id, port)? {
        return Err(format!("port {port} is configured for extension {owner}"));
    }
    if allow_current {
        return Ok(());
    }
    let listeners = listening_tcp_ports()?;
    if listeners.contains(&port) {
        return Err(format!("port {port} is already used by a TCP service"));
    }
    Ok(())
}

fn setting_port(settings: &Value) -> Option<i64> {
    let value = settings.get("port")?.as_f64()?;
    if value < 1.0 || value > 65535.0 || value.fract() != 0.0 {
        return None;
    }
    Some(value as i64)
}

fn configured_web_port() -> Result<i64, String> {
    let data = match fs::read(paths::ONEKVM_CONFIG) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(80),
        Err(err) => return Err(format!("read OneKVM configuration: {err}")),
    };
    let value: Value =
        serde_json::from_slice(&data).map_err(|e| format!("decode OneKVM configuration: {e}"))?;
    Ok(value
        .pointer("/network/http_port")
        .and_then(Value::as_i64)
        .filter(|port| *port > 0)
        .unwrap_or(80))
}

fn configured_owner(exclude_id: &str, port: i64) -> Result<Option<String>, String> {
    let entries = match fs::read_dir(paths::CONFIG_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("read extension configurations: {err}")),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) || !name.ends_with(".json") {
            continue;
        }
        let id = name.trim_end_matches(".json");
        if id == exclude_id || !paths::valid_id(id) {
            continue;
        }
        let state = read_state(id)?;
        if setting_port(&Value::Object(state.settings.clone().into_iter().collect())) == Some(port) {
            return Ok(Some(id.to_owned()));
        }
    }
    Ok(None)
}

fn listening_tcp_ports() -> Result<std::collections::BTreeSet<i64>, String> {
    let mut ports = std::collections::BTreeSet::new();
    for filename in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let file = match fs::File::open(Path::new(filename)) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("read TCP listeners: {err}")),
        };
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| format!("read TCP listeners: {e}"))?;
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 4 || fields[3] != "0A" {
                continue;
            }
            if let Some(encoded) = fields[1].split(':').nth(1) {
                if let Ok(port) = i64::from_str_radix(encoded, 16) {
                    ports.insert(port);
                }
            }
        }
    }
    Ok(ports)
}

pub fn contains_changed_key(keys: &[String], wanted: &str) -> bool {
    keys.iter().any(|key| key == wanted)
}
