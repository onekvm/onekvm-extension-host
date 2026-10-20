use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::control::parse_control;
use crate::manifest::{decode_manifest, Manifest};
use crate::paths;
use crate::sys;

const MAX_FEED_ICON_DATA_URL: usize = 384 * 1024;
const MAX_CUSTOM_FEED_BYTES: usize = 16 * 1024;
const MAX_SOURCE_DISPLAY_NAME: usize = 64;
const SOURCE_KEY_PREFIX: &str = "# OneKVM-Source-Key:";
const SOURCE_NAME_PREFIX: &str = "# OneKVM-Source-Name:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedSource {
    pub key: String,
    pub name: String,
    pub path: String,
    pub feeds_text: String,
    pub builtin: bool,
    pub enabled: bool,
}

#[derive(Clone)]
struct ConfiguredUrl {
    name: String,
    scheme: String,
    host: String,
    path: String,
}

struct FeedPolicy {
    metadata_list: String,
    package_lists: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AvailableExtension {
    pub package_version: String,
    pub package_dependencies: Vec<String>,
    pub manifest: Manifest,
    pub icon_data_url: String,
    pub size_bytes: u64,
    pub installed_size_bytes: u64,
}

pub fn available_extensions() -> BTreeMap<String, AvailableExtension> {
    let Ok(policy) = load_feed_policy(paths::TRUSTED_FEEDS) else {
        return BTreeMap::new();
    };
    if validate_feed_freshness(&policy.metadata_list, paths::OPKG_LISTS, SystemTime::now()).is_err()
    {
        let err = crate::sys::with_lock(|| {
            if validate_feed_freshness(&policy.metadata_list, paths::OPKG_LISTS, SystemTime::now())
                .is_ok()
            {
                return Ok(());
            }
            sys::opkg(&["update"])?;
            relax_opkg_list_permissions();
            validate_feed_freshness(&policy.metadata_list, paths::OPKG_LISTS, SystemTime::now())
        });
        if err.is_err() {
            return BTreeMap::new();
        }
    }
    let Ok(index) = crate::package_sizes::PackageIndex::load(
        Path::new(paths::OPKG_LISTS),
        &policy.package_lists,
    ) else {
        return BTreeMap::new();
    };
    let mut result: BTreeMap<String, AvailableExtension> = BTreeMap::new();
    for package in index.extensions() {
        let Ok(mut extension) = available_from_package_fields(&package.fields) else {
            continue;
        };
        (extension.size_bytes, extension.installed_size_bytes) = index.sizes(package);
        match result.get(&extension.manifest.id) {
            Some(current)
                if !crate::control::compare_versions(
                    &extension.package_version,
                    &current.package_version,
                )
                .is_gt() => {}
            _ => {
                result.insert(extension.manifest.id.clone(), extension);
            }
        }
    }
    result
}

fn available_from_package_fields(
    fields: &BTreeMap<String, String>,
) -> Result<AvailableExtension, String> {
    let manifest = manifest_from_package_fields(fields)?;
    Ok(AvailableExtension {
        package_version: fields.get("Version").cloned().unwrap_or_default(),
        package_dependencies: crate::control::package_dependencies(fields),
        icon_data_url: icon_data_url_from_package_fields(fields),
        size_bytes: package_size_field(fields, "Size"),
        installed_size_bytes: package_size_field(fields, "Installed-Size"),
        manifest,
    })
}

fn package_size_field(fields: &BTreeMap<String, String>, key: &str) -> u64 {
    fields
        .get(key)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn icon_data_url_from_package_fields(fields: &BTreeMap<String, String>) -> String {
    let Some(value) = fields.get("X-OneKVM-Extension-Icon") else {
        return String::new();
    };
    if value.len() > MAX_FEED_ICON_DATA_URL || !valid_icon_data_url(value) {
        return String::new();
    }
    value.clone()
}

fn valid_icon_data_url(value: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "data:image/svg+xml;base64,",
        "data:image/png;base64,",
        "data:image/webp;base64,",
    ];
    let Some(prefix) = PREFIXES.iter().find(|prefix| value.starts_with(*prefix)) else {
        return false;
    };
    let encoded = &value[prefix.len()..];
    !encoded.is_empty()
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn manifest_from_package_fields(fields: &BTreeMap<String, String>) -> Result<Manifest, String> {
    let encoded = fields
        .get("X-OneKVM-Extension-Manifest")
        .ok_or_else(|| "extension package metadata has no manifest".to_string())?;
    let data = decode_base64(encoded)?;
    if data.len() > 64 * 1024 {
        return Err("invalid extension manifest metadata".into());
    }
    let extension = decode_manifest(&data)?;
    let package_name = format!("onekvm-extension-{}", extension.id);
    if fields.get("Package").map(String::as_str) != Some(package_name.as_str())
        || fields.get("X-OneKVM-Extension-Version").map(String::as_str)
            != Some(extension.version.as_str())
    {
        return Err("extension manifest metadata does not match package identity".into());
    }
    Ok(extension)
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if cleaned.len() % 4 != 0 {
        return Err("invalid extension manifest metadata".into());
    }
    let mut out = Vec::new();
    for chunk in cleaned.chunks(4) {
        let a = value(chunk[0]).ok_or_else(|| "invalid extension manifest metadata".to_string())?;
        let b = value(chunk[1]).ok_or_else(|| "invalid extension manifest metadata".to_string())?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            value(chunk[2]).ok_or_else(|| "invalid extension manifest metadata".to_string())?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            value(chunk[3]).ok_or_else(|| "invalid extension manifest metadata".to_string())?
        };
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

pub fn refresh() -> Result<(), String> {
    let policy = load_feed_policy(paths::TRUSTED_FEEDS)?;
    sys::opkg(&["update"]).map_err(|err| format!("signed feed update failed: {err}"))?;
    relax_opkg_list_permissions();
    validate_feed_freshness(&policy.metadata_list, paths::OPKG_LISTS, SystemTime::now())
        .map_err(|err| format!("reject package feed: {err}"))
}

pub fn refresh_if_stale() -> Result<(), String> {
    let policy = load_feed_policy(paths::TRUSTED_FEEDS)?;
    if validate_feed_freshness(&policy.metadata_list, paths::OPKG_LISTS, SystemTime::now()).is_ok()
    {
        return Ok(());
    }
    refresh()
}

fn relax_opkg_list_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = fs::read_dir(paths::OPKG_LISTS) else {
        return;
    };
    for entry in entries.flatten() {
        let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o644));
    }
}

pub fn validate_feed_freshness(
    metadata_list: &str,
    lists_root: &str,
    now: SystemTime,
) -> Result<(), String> {
    let data = fs::read(Path::new(lists_root).join(metadata_list))
        .map_err(|_| "read signed feed metadata".to_string())?;
    let text = String::from_utf8_lossy(&data);
    for paragraph in text.split("\n\n") {
        if paragraph.trim().is_empty() {
            continue;
        }
        let Ok(fields) = parse_control(paragraph.as_bytes()) else {
            continue;
        };
        if fields.get("Package").map(String::as_str) != Some("onekvm-feed-metadata") {
            continue;
        }
        let value = fields
            .get("X-OneKVM-Feed-Valid-Until")
            .ok_or_else(|| "invalid feed expiration \"\"".to_string())?;
        let valid_until = parse_rfc3339(value)?;
        if now >= valid_until {
            return Err(format!("feed expired at {value}"));
        }
        return Ok(());
    }
    Err("signed feed metadata package is missing".into())
}

fn parse_rfc3339(value: &str) -> Result<SystemTime, String> {
    // RFC3339 UTC timestamps used by the signed feed, e.g. 2026-01-02T03:04:05Z
    let trimmed = value.strip_suffix('Z').unwrap_or(value);
    let (date, time) = trimmed
        .split_once('T')
        .ok_or_else(|| format!("invalid feed expiration {value:?}"))?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts
        .next()
        .and_then(|item| item.parse().ok())
        .ok_or_else(|| format!("invalid feed expiration {value:?}"))?;
    let month: u32 = date_parts
        .next()
        .and_then(|item| item.parse().ok())
        .ok_or_else(|| format!("invalid feed expiration {value:?}"))?;
    let day: u32 = date_parts
        .next()
        .and_then(|item| item.parse().ok())
        .ok_or_else(|| format!("invalid feed expiration {value:?}"))?;
    let (hms, _) = time.split_once(['+', '-']).unwrap_or((time, ""));
    let mut time_parts = hms.split(':');
    let hour: u32 = time_parts
        .next()
        .and_then(|item| item.parse().ok())
        .ok_or_else(|| format!("invalid feed expiration {value:?}"))?;
    let minute: u32 = time_parts
        .next()
        .and_then(|item| item.parse().ok())
        .ok_or_else(|| format!("invalid feed expiration {value:?}"))?;
    let second: u32 = time_parts
        .next()
        .and_then(|item| item.parse().ok())
        .ok_or_else(|| format!("invalid feed expiration {value:?}"))?;
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    if secs < 0 {
        return Err(format!("invalid feed expiration {value:?}"));
    }
    Ok(UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Result<i64, String> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err("invalid feed expiration".into());
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(i64::from(era) * 146097 + i64::from(doe) - 719468)
}

fn load_feed_policy(trusted_name: &str) -> Result<FeedPolicy, String> {
    let prefixes = read_configured_urls(Path::new(trusted_name), false)?;
    if prefixes.is_empty() {
        return Err("no trusted feed prefixes are configured".into());
    }
    let feeds = read_feed_config_urls()?;
    if feeds.is_empty() {
        return Err("no package feed is configured".into());
    }
    let mut policy = FeedPolicy {
        metadata_list: String::new(),
        package_lists: Vec::new(),
    };
    for feed in feeds {
        if !prefixes.iter().any(|prefix| same_url_prefix(prefix, &feed)) {
            return Err(format!(
                "feed URL {}://{}{} is not trusted",
                feed.scheme, feed.host, feed.path
            ));
        }
        if feed.name == "onekvm-metadata" {
            if !policy.metadata_list.is_empty() {
                return Err("multiple onekvm-metadata feeds are configured".into());
            }
            policy.metadata_list = feed.name;
        } else {
            policy.package_lists.push(feed.name);
        }
    }
    if policy.metadata_list.is_empty() {
        return Err("signed onekvm-metadata feed is required".into());
    }
    Ok(policy)
}

fn read_feed_config_urls() -> Result<Vec<ConfiguredUrl>, String> {
    let mut values = Vec::new();
    for path in paths::feed_config_paths() {
        values.extend(read_configured_urls(&path, true)?);
    }
    Ok(values)
}

fn read_configured_urls(filename: &Path, feed_syntax: bool) -> Result<Vec<ConfiguredUrl>, String> {
    let file = fs::File::open(filename).map_err(|e| e.to_string())?;
    let mut values = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = if feed_syntax {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() != 3 || (fields[0] != "src" && fields[0] != "src/gz") {
                return Err(format!(
                    "feed configuration contains a forbidden line: {line:?}"
                ));
            }
            (fields[1].to_owned(), fields[2].to_owned())
        } else {
            (String::new(), line.to_owned())
        };
        values.push(parse_url(&name, &value)?);
    }
    Ok(values)
}

fn parse_url(name: &str, value: &str) -> Result<ConfiguredUrl, String> {
    let Some((scheme, rest)) = value.split_once("://") else {
        return Err(format!("invalid feed URL {value:?}"));
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!("invalid feed URL {value:?}"));
    }
    if rest.contains('@') || rest.contains('?') || rest.contains('#') {
        return Err(format!("invalid feed URL {value:?}"));
    }
    let (host_path, _) = rest
        .split_once('/')
        .map(|(h, p)| (h, p))
        .unwrap_or((rest, ""));
    let host = if rest.contains('/') {
        rest.split('/').next().unwrap_or_default()
    } else {
        host_path
    };
    if host.is_empty() {
        return Err(format!("invalid feed URL {value:?}"));
    }
    let path = if let Some((_, path)) = rest.split_once('/') {
        format!("/{path}")
    } else {
        "/".into()
    };
    Ok(ConfiguredUrl {
        name: name.to_owned(),
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        path,
    })
}

pub fn custom_feed_path(name: &str) -> Result<PathBuf, String> {
    if !paths::valid_feed_source_key(name) {
        return Err("invalid source key".into());
    }
    Ok(Path::new(paths::OPKG_CONF_DIR).join(format!(
        "{}{}{}",
        paths::FEED_CUSTOM_PREFIX,
        name,
        paths::FEED_CUSTOM_SUFFIX
    )))
}

pub fn valid_feed_source_display_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("source name is required".into());
    }
    if name.chars().count() > MAX_SOURCE_DISPLAY_NAME {
        return Err("source name is too long".into());
    }
    if name
        .chars()
        .any(|c| c == '\n' || c == '\r' || c == '\0' || c == '\u{2028}' || c == '\u{2029}')
    {
        return Err("invalid source name".into());
    }
    Ok(())
}

fn parse_source_metadata(text: &str) -> (Option<String>, Option<String>, String) {
    let mut key = None;
    let mut name = None;
    let mut rest = String::new();
    let mut in_header = true;
    for line in text.lines() {
        if in_header {
            let trimmed = line.trim();
            if let Some(value) = trimmed.strip_prefix(SOURCE_KEY_PREFIX) {
                let value = value.trim();
                if !value.is_empty() {
                    key = Some(value.to_owned());
                }
                continue;
            }
            if let Some(value) = trimmed.strip_prefix(SOURCE_NAME_PREFIX) {
                let value = value.trim();
                if !value.is_empty() {
                    name = Some(value.to_owned());
                }
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            in_header = false;
        }
        rest.push_str(line);
        rest.push('\n');
    }
    (key, name, rest)
}

fn encode_source_file(key: &str, name: &str, feeds_text: &str) -> String {
    let (_, _, feeds) = parse_source_metadata(feeds_text);
    let mut out = format!("{SOURCE_KEY_PREFIX} {key}\n{SOURCE_NAME_PREFIX} {name}\n");
    if !feeds.is_empty() {
        out.push_str(&feeds);
        if !feeds.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn read_source_file(
    file: &Path,
    listed_path: &Path,
    key: &str,
    builtin: bool,
    enabled: bool,
) -> Result<Option<FeedSource>, String> {
    match fs::read_to_string(file) {
        Ok(raw) => {
            if builtin {
                return Ok(Some(FeedSource {
                    key: key.to_owned(),
                    name: String::new(),
                    path: listed_path.to_string_lossy().into_owned(),
                    feeds_text: raw,
                    builtin,
                    enabled,
                }));
            }
            let (_, display, feeds_text) = parse_source_metadata(&raw);
            let name = display
                .filter(|value| valid_feed_source_display_name(value).is_ok())
                .unwrap_or_else(|| key.to_owned());
            Ok(Some(FeedSource {
                key: key.to_owned(),
                name,
                path: listed_path.to_string_lossy().into_owned(),
                feeds_text,
                builtin,
                enabled,
            }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("read {}: {err}", file.display())),
    }
}

pub fn list_feed_sources() -> Result<Vec<FeedSource>, String> {
    let mut sources = Vec::new();
    let system = Path::new(paths::FEED_CONFIG);
    if let Some(source) = read_source_file(system, system, paths::BUILTIN_FEED_NAME, true, true)? {
        sources.push(source);
    } else if let Some(source) = read_source_file(
        &paths::disabled_feed_path(system),
        system,
        paths::BUILTIN_FEED_NAME,
        true,
        false,
    )? {
        sources.push(source);
    }

    let Ok(entries) = fs::read_dir(paths::OPKG_CONF_DIR) else {
        return Ok(sources);
    };
    let custom_disabled_suffix = format!(
        "{}{}",
        paths::FEED_CUSTOM_SUFFIX,
        paths::FEED_DISABLED_SUFFIX
    );
    let mut custom: BTreeMap<String, FeedSource> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let (key, enabled) = if let Some(stem) = file_name
            .strip_prefix(paths::FEED_CUSTOM_PREFIX)
            .and_then(|value| value.strip_suffix(paths::FEED_CUSTOM_SUFFIX))
        {
            (stem, true)
        } else if let Some(stem) = file_name
            .strip_prefix(paths::FEED_CUSTOM_PREFIX)
            .and_then(|value| value.strip_suffix(custom_disabled_suffix.as_str()))
        {
            (stem, false)
        } else {
            continue;
        };
        if !paths::valid_feed_source_key(key) {
            continue;
        }
        let listed = custom_feed_path(key)?;
        if enabled {
            if let Some(source) = read_source_file(&path, &listed, key, false, true)? {
                custom.insert(key.to_owned(), source);
            }
            continue;
        }
        if custom.contains_key(key) {
            continue;
        }
        if let Some(source) = read_source_file(&path, &listed, key, false, false)? {
            custom.insert(key.to_owned(), source);
        }
    }
    sources.extend(custom.into_values());
    Ok(sources)
}

fn set_path_enabled(active: &Path, enabled: bool) -> Result<(), String> {
    let disabled = paths::disabled_feed_path(active);
    if enabled {
        if active.is_file() {
            if disabled.is_file() {
                fs::remove_file(&disabled)
                    .map_err(|err| format!("remove {}: {err}", disabled.display()))?;
            }
            return Ok(());
        }
        if !disabled.is_file() {
            return Err("package source is missing".into());
        }
        return fs::rename(&disabled, active)
            .map_err(|err| format!("enable {}: {err}", active.display()));
    }
    if disabled.is_file() && !active.is_file() {
        return Ok(());
    }
    if !active.is_file() {
        return Err("package source is missing".into());
    }
    if disabled.is_file() {
        fs::remove_file(&disabled)
            .map_err(|err| format!("remove {}: {err}", disabled.display()))?;
    }
    fs::rename(active, &disabled).map_err(|err| format!("disable {}: {err}", active.display()))
}

pub fn set_builtin_source_enabled(enabled: bool) -> Result<(), String> {
    set_path_enabled(Path::new(paths::FEED_CONFIG), enabled)
}

pub fn set_custom_source_enabled(name: &str, enabled: bool) -> Result<(), String> {
    set_path_enabled(&custom_feed_path(name)?, enabled)
}

pub fn set_custom_source(key: &str, name: &str, feeds_text: &str) -> Result<(), String> {
    let name = name.trim();
    valid_feed_source_display_name(name)?;
    let path = custom_feed_path(key)?;
    let (_, _, stripped) = parse_source_metadata(feeds_text);
    validate_custom_feed_text(&stripped)?;
    let encoded = encode_source_file(key, name, &stripped);
    sys::atomic_write(&path, encoded.as_bytes(), 0o644)?;
    let disabled = paths::disabled_feed_path(&path);
    if disabled.is_file() {
        fs::remove_file(&disabled)
            .map_err(|err| format!("remove {}: {err}", disabled.display()))?;
    }
    Ok(())
}

pub fn remove_custom_source(name: &str) -> Result<(), String> {
    let path = custom_feed_path(name)?;
    let disabled = paths::disabled_feed_path(&path);
    for candidate in [&path, &disabled] {
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("unlink {}: {err}", candidate.display())),
        }
    }
    Ok(())
}

pub fn validate_custom_feed_text(text: &str) -> Result<(), String> {
    if text.len() > MAX_CUSTOM_FEED_BYTES {
        return Err(format!(
            "feed configuration exceeds {MAX_CUSTOM_FEED_BYTES} bytes"
        ));
    }
    let mut saw_feed = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.len() >= 512 {
            return Err("feed line too long".into());
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 3 {
            return Err("invalid feed line".into());
        }
        if fields[0] != "src" && fields[0] != "src/gz" {
            return Err("feed kind must be src or src/gz".into());
        }
        let name = fields[1];
        if name.contains('/') || name.starts_with('.') {
            return Err("invalid feed name".into());
        }
        parse_url(name, fields[2])?;
        saw_feed = true;
    }
    if !saw_feed {
        return Err("source must contain at least one src/gz line".into());
    }
    Ok(())
}

fn same_url_prefix(prefix: &ConfiguredUrl, feed: &ConfiguredUrl) -> bool {
    if !prefix.scheme.eq_ignore_ascii_case(&feed.scheme)
        || !prefix.host.eq_ignore_ascii_case(&feed.host)
    {
        return false;
    }
    let prefix_path = prefix.path.trim_end_matches('/').to_owned();
    let feed_path = if feed.path.is_empty() {
        "/".to_owned()
    } else {
        feed.path.clone()
    };
    prefix_path.is_empty()
        || feed_path == prefix_path
        || feed_path.starts_with(&(prefix_path + "/"))
}

#[cfg(test)]
mod tests {
    use super::{
        custom_feed_path, encode_source_file, icon_data_url_from_package_fields,
        manifest_from_package_fields, parse_source_metadata, set_path_enabled,
        valid_feed_source_display_name, valid_icon_data_url, validate_custom_feed_text,
    };
    use crate::paths;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn feed_manifest_must_match_package_identity() {
        let mut fields = BTreeMap::new();
        fields.insert("Package".into(), "onekvm-extension-edid".into());
        fields.insert("X-OneKVM-Extension-Version".into(), "1.0.0".into());
        fields.insert("X-OneKVM-Extension-Manifest".into(), "not-base64".into());
        assert!(manifest_from_package_fields(&fields).is_err());
    }

    #[test]
    fn feed_icon_data_url_accepts_image_base64() {
        assert!(valid_icon_data_url("data:image/png;base64,abc+/="));
        assert!(!valid_icon_data_url("data:text/plain;base64,abc"));
        let mut fields = BTreeMap::new();
        fields.insert(
            "X-OneKVM-Extension-Icon".into(),
            "data:image/svg+xml;base64,PHN2Zy8+".into(),
        );
        assert_eq!(
            icon_data_url_from_package_fields(&fields),
            "data:image/svg+xml;base64,PHN2Zy8+"
        );
    }

    #[test]
    fn source_metadata_round_trip_keeps_display_name() {
        let encoded = encode_source_file("lab", "内部镜像", "src foo https://example.org/a\n");
        assert_eq!(
            encoded,
            "# OneKVM-Source-Key: lab\n# OneKVM-Source-Name: 内部镜像\nsrc foo https://example.org/a\n"
        );
        let (key, name, feeds) = parse_source_metadata(&encoded);
        assert_eq!(key.as_deref(), Some("lab"));
        assert_eq!(name.as_deref(), Some("内部镜像"));
        assert_eq!(feeds, "src foo https://example.org/a\n");
    }

    #[test]
    fn parse_source_metadata_leaves_legacy_files_alone() {
        let (key, name, feeds) = parse_source_metadata("src foo https://example.org/a\n");
        assert_eq!(key, None);
        assert_eq!(name, None);
        assert_eq!(feeds, "src foo https://example.org/a\n");
    }

    #[test]
    fn parse_source_metadata_strips_pasted_headers() {
        let text = "# OneKVM-Source-Key: old\n# OneKVM-Source-Name: 旧名\n\nsrc foo https://example.org/a\n";
        let (_, _, feeds) = parse_source_metadata(text);
        assert_eq!(feeds, "src foo https://example.org/a\n");
        let rewritten = encode_source_file("lab", "内部镜像", text);
        assert_eq!(
            rewritten,
            "# OneKVM-Source-Key: lab\n# OneKVM-Source-Name: 内部镜像\nsrc foo https://example.org/a\n"
        );
    }

    #[test]
    fn display_name_and_key_validation() {
        assert!(valid_feed_source_display_name("内部镜像").is_ok());
        assert!(valid_feed_source_display_name("  lab  ").is_ok());
        assert!(valid_feed_source_display_name("").is_err());
        assert!(valid_feed_source_display_name("a\nb").is_err());
        assert!(custom_feed_path("lab").is_ok());
        assert_eq!(custom_feed_path("Bad").unwrap_err(), "invalid source key");
    }

    #[test]
    fn custom_feed_text_matches_marketplace_cli_rules() {
        assert!(validate_custom_feed_text(
            "src onekvm-riscv64imafdc https://updates.example.org/nanokvm/0.2.0/riscv64imafdc\n"
        )
        .is_ok());
        assert!(validate_custom_feed_text(
            "# comment\n\nsrc/gz mirror http://10.100.99.70:18080/nanokvm/0.1.0/main\n"
        )
        .is_ok());
        assert!(validate_custom_feed_text("").is_err());
        assert!(validate_custom_feed_text("src bad\n").is_err());
        assert!(validate_custom_feed_text("dest foo https://example.org/feed\n").is_err());
        assert!(validate_custom_feed_text("src .hidden https://example.org/feed\n").is_err());
        assert!(validate_custom_feed_text("src foo ftp://example.org/feed\n").is_err());
        assert!(validate_custom_feed_text("src foo https://user:pass@example.org/feed\n").is_err());
    }

    fn unique_temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onekvm-feed-src-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn set_path_enabled_renames_between_conf_and_disabled() {
        let dir = unique_temp_dir();
        let active = dir.join("onekvm-feed.conf");
        fs::write(&active, "src onekvm https://example.org/feed\n").unwrap();
        set_path_enabled(&active, false).unwrap();
        assert!(!active.is_file());
        let disabled = paths::disabled_feed_path(&active);
        assert!(disabled.is_file());
        assert_eq!(
            fs::read_to_string(&disabled).unwrap(),
            "src onekvm https://example.org/feed\n"
        );
        set_path_enabled(&active, true).unwrap();
        assert!(active.is_file());
        assert!(!disabled.is_file());
        let _ = fs::remove_dir_all(&dir);
    }
}
