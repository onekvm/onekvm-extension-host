//! Read the current system policy for every child, including a long-lived host.
use std::path::Path;
use std::process::Command;

const KEYS: [&str; 6] = [
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "no_proxy",
    "NO_PROXY",
];

pub fn apply(command: &mut Command) -> Result<(), String> {
    apply_at(command, Path::new("/etc/onekvm/config.json"))
}

fn apply_at(command: &mut Command, path: &Path) -> Result<(), String> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("read system proxy configuration".into()),
    };
    let config: serde_json::Value =
        serde_json::from_slice(&data).map_err(|_| "parse system proxy configuration")?;
    let proxy = &config["network"]["proxy"];
    let http = proxy["http"].as_str().unwrap_or("");
    let https = proxy["https"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(http);
    let no_proxy = proxy["no_proxy"].as_str().unwrap_or("");
    for (key, value) in KEYS
        .into_iter()
        .zip([http, http, https, https, no_proxy, no_proxy])
    {
        command.env_remove(key);
        if !value.is_empty() {
            command.env(key, value);
        }
    }
    // An inherited catch-all must not override an explicitly cleared policy.
    command.env_remove("all_proxy").env_remove("ALL_PROXY");
    Ok(())
}

/// Empty assignments override the helper's potentially stale inherited policy.
pub fn environment(existing: &[String]) -> Result<Vec<String>, String> {
    let mut command = Command::new("/bin/true");
    apply(&mut command)?;
    let mut result: Vec<String> = existing
        .iter()
        .filter(|entry| {
            !entry.split_once('=').is_some_and(|(key, _)| {
                KEYS.contains(&key) || matches!(key, "all_proxy" | "ALL_PROXY")
            })
        })
        .cloned()
        .collect();
    for (key, value) in command.get_envs() {
        result.push(format!(
            "{}={}",
            key.to_string_lossy(),
            value.unwrap_or_default().to_string_lossy()
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_running_host_children_follow_changes_and_clears() {
        let path =
            std::env::temp_dir().join(format!("onekvm-proxy-child-{}.json", std::process::id()));
        for (proxy, expected) in [
            (
                r#"{"http":"http://first:3128","no_proxy":"localhost"}"#,
                "http://first:3128|http://first:3128|localhost",
            ),
            (
                r#"{"http":"http://second:3128","https":"http://tls:3128"}"#,
                "http://second:3128|http://tls:3128|",
            ),
            ("{}", "||"),
        ] {
            std::fs::write(&path, format!(r#"{{"network":{{"proxy":{proxy}}}}}"#)).unwrap();
            let mut command = Command::new("/bin/sh");
            command
                .env("http_proxy", "stale")
                .env("https_proxy", "stale")
                .env("no_proxy", "stale");
            command.args([
                "-c",
                "printf '%s|%s|%s' \"$http_proxy\" \"$https_proxy\" \"$no_proxy\"",
            ]);
            apply_at(&mut command, &path).unwrap();
            assert_eq!(
                String::from_utf8(command.output().unwrap().stdout).unwrap(),
                expected
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
