use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::control::parse_control;
use crate::manifest::{decode_manifest, Manifest};
use crate::paths;
use crate::sys;

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

pub fn available_extensions() -> BTreeMap<String, Manifest> {
    let Ok(policy) = load_feed_policy(paths::FEED_CONFIG, paths::TRUSTED_FEEDS) else {
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
            validate_feed_freshness(&policy.metadata_list, paths::OPKG_LISTS, SystemTime::now())
        });
        if err.is_err() {
            return BTreeMap::new();
        }
    }
    let mut result: BTreeMap<String, Manifest> = BTreeMap::new();
    for list in &policy.package_lists {
        let Ok(data) = fs::read(Path::new(paths::OPKG_LISTS).join(list)) else {
            continue;
        };
        for paragraph in String::from_utf8_lossy(&data).split("\n\n") {
            let Ok(fields) = parse_control(paragraph.as_bytes()) else {
                continue;
            };
            let Some(package) = fields.get("Package") else {
                continue;
            };
            if !paths::valid_package_name(package) {
                continue;
            }
            let Ok(extension) = manifest_from_package_fields(&fields) else {
                continue;
            };
            match result.get(&extension.id) {
                Some(current) if extension.version <= current.version => {}
                _ => {
                    result.insert(extension.id.clone(), extension);
                }
            }
        }
    }
    result
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
    let policy = load_feed_policy(paths::FEED_CONFIG, paths::TRUSTED_FEEDS)?;
    sys::opkg(&["update"]).map_err(|err| format!("signed feed update failed: {err}"))?;
    validate_feed_freshness(&policy.metadata_list, paths::OPKG_LISTS, SystemTime::now())
        .map_err(|err| format!("reject package feed: {err}"))
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

fn load_feed_policy(config_name: &str, trusted_name: &str) -> Result<FeedPolicy, String> {
    let prefixes = read_configured_urls(trusted_name, false)?;
    if prefixes.is_empty() {
        return Err("no trusted feed prefixes are configured".into());
    }
    let feeds = read_configured_urls(config_name, true)?;
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

fn read_configured_urls(filename: &str, feed_syntax: bool) -> Result<Vec<ConfiguredUrl>, String> {
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
    use super::manifest_from_package_fields;
    use std::collections::BTreeMap;

    #[test]
    fn feed_manifest_must_match_package_identity() {
        let mut fields = BTreeMap::new();
        fields.insert("Package".into(), "onekvm-extension-edid".into());
        fields.insert("X-OneKVM-Extension-Version".into(), "1.0.0".into());
        fields.insert("X-OneKVM-Extension-Manifest".into(), "not-base64".into());
        assert!(manifest_from_package_fields(&fields).is_err());
    }
}
