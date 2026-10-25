//! Identity supplied by the signed root filesystem, independent of its release
//! string and system build ID. Missing legacy metadata never proves equality.
use std::collections::BTreeMap;
use std::fs;

use serde::Deserialize;

pub const METADATA: &str = "/usr/share/onekvm/kernel.json";

#[derive(Debug, Deserialize)]
pub struct Identity {
    schema: u32,
    pub release: String,
    pub machine: String,
    pub magic: String,
}

pub fn valid_magic(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

pub fn decode(data: &[u8], release: &str) -> Result<Identity, String> {
    let value: Identity =
        serde_json::from_slice(data).map_err(|e| format!("invalid image kernel metadata: {e}"))?;
    if value.schema != 1
        || value.release != release
        || !valid_magic(&value.magic)
        || value.machine.is_empty()
    {
        return Err("image kernel metadata does not match its kernel release or schema".into());
    }
    Ok(value)
}

pub fn current_magic() -> Result<String, String> {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").map_err(|e| e.to_string())?;
    match fs::read(METADATA) {
        Ok(data) => Ok(decode(&data, release.trim())?.magic),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("read image kernel metadata: {e}")),
    }
}

pub fn changed(
    source_release: &str,
    source_magic: &str,
    target_release: &str,
    target_magic: &str,
) -> bool {
    source_release != target_release
        || !valid_magic(source_magic)
        || !valid_magic(target_magic)
        || source_magic != target_magic
}

pub fn check_package(
    fields: &BTreeMap<String, String>,
    release: &str,
    magic: &str,
) -> Result<(), String> {
    let name = fields.get("Package").map(String::as_str).unwrap_or("");
    if !needs_identity(name) && !fields.contains_key("X-OneKVM-Kernel-Magic") {
        return Ok(());
    }
    if !valid_magic(magic) {
        return Err("kernel module compatibility is unknown: update the sqfs system image to obtain kernel magic metadata".into());
    }
    if fields.get("X-OneKVM-Kernel-Release").map(String::as_str) != Some(release)
        || fields.get("X-OneKVM-Kernel-Magic").map(String::as_str) != Some(magic)
    {
        return Err(format!("kernel module package {name} does not match the image kernel magic; obtain modules built for this system image"));
    }
    Ok(())
}

fn needs_identity(name: &str) -> bool {
    name.contains("kernel-module")
        || name == "onekvm-nanokvm-oled"
        || name.ends_with("-wireless-modules")
        || name == "onekvm-video-soph-mmf-modules"
}

pub fn check_current_package(fields: &BTreeMap<String, String>) -> Result<(), String> {
    // Userspace-only installations must continue to work on legacy images.
    let name = fields.get("Package").map(String::as_str).unwrap_or("");
    if !needs_identity(name) && !fields.contains_key("X-OneKVM-Kernel-Magic") {
        return Ok(());
    }
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").map_err(|e| e.to_string())?;
    check_package(fields, release.trim(), &current_magic()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn magic(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    #[test]
    fn same_release_rebuild_and_missing_legacy_metadata_require_validation() {
        assert!(changed("6.18", &magic('a'), "6.18", &magic('b')));
        assert!(changed("6.18", "", "6.18", ""));
        assert!(!changed("6.18", &magic('a'), "6.18", &magic('a')));
        assert!(changed("6.19", &magic('a'), "6.18", &magic('a')));
    }

    #[test]
    fn module_packages_require_exact_image_identity() {
        let mut fields = BTreeMap::from([("Package".into(), "onekvm-kernel-module-tun".into())]);
        assert!(check_package(&fields, "6.18", &magic('a')).is_err());
        fields.insert("X-OneKVM-Kernel-Release".into(), "6.18".into());
        fields.insert("X-OneKVM-Kernel-Magic".into(), magic('a'));
        assert!(check_package(&fields, "6.18", &magic('a')).is_ok());
        assert!(check_package(&fields, "6.18", &magic('b')).is_err());
        assert!(check_package(&fields, "6.18", "").is_err());
        fields.insert("Package".into(), "optional-runtime".into());
        assert!(check_package(&fields, "6.18", &magic('b')).is_err());
    }

    #[test]
    fn metadata_rejects_wrong_release_schema_and_invalid_magic() {
        let mut data =
            serde_json::json!({"schema":1,"release":"6.18","machine":"nano","magic":magic('a')});
        assert!(decode(&serde_json::to_vec(&data).unwrap(), "6.18").is_ok());
        assert!(decode(&serde_json::to_vec(&data).unwrap(), "6.19").is_err());
        data["schema"] = 2.into();
        assert!(decode(&serde_json::to_vec(&data).unwrap(), "6.18").is_err());
        data["schema"] = 1.into();
        data["magic"] = "sha256:../../old".into();
        assert!(decode(&serde_json::to_vec(&data).unwrap(), "6.18").is_err());
    }
}
