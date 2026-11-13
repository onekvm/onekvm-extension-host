//! Shared installer policy format; absent policies retain existing behavior.
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    disabled: Vec<String>,
}

fn validate(data: &[u8], feature: &str) -> Result<(), String> {
    let policy: Policy =
        serde_json::from_slice(data).map_err(|_| "invalid installed network policy")?;
    let known = [
        "internet",
        "ntp",
        "dns",
        "discovery",
        "stun",
        "turn",
        "acme",
        "update",
        "extensions",
        "mdns",
    ];
    let mut seen = std::collections::HashSet::new();
    if policy.version != 1
        || policy.disabled.len() > known.len()
        || policy
            .disabled
            .iter()
            .any(|s| !known.contains(&s.as_str()) || !seen.insert(s))
    {
        return Err("invalid installed network policy".into());
    }
    if policy.disabled.iter().any(|s| s == feature) {
        return Err(format!(
            "{feature} is disabled by the installed network policy"
        ));
    }
    Ok(())
}

pub fn require(feature: &str) -> Result<(), String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/etc/onekvm/network-policy.json")
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("cannot read network policy: {err}")),
    };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("invalid installed network policy file".into());
    }
    let mut data = Vec::new();
    file.take(16385)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > 16384 {
        return Err("oversized network policy".into());
    }
    validate(&data, feature)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opt_out_blocks_remote_packages_but_validates_the_entire_policy() {
        assert!(validate(br#"{"version":1,"disabled":["extensions"]}"#, "extensions").is_err());
        assert!(validate(br#"{"version":1,"disabled":["internet"]}"#, "extensions").is_ok());
        assert!(validate(br#"{"version":2,"disabled":[]}"#, "extensions").is_err());
        assert!(validate(br#"{"version":1,"disabled":["typo"]}"#, "extensions").is_err());
    }
}
