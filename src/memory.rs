use std::fs;
use std::io::{BufRead, BufReader};

use serde::Serialize;

use crate::manifest::{load_installed, Manifest};
use crate::paths;
use crate::state::read_state;

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct MemoryBudgetStatus {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

pub fn parse_memory_size(value: &str) -> Result<u64, String> {
    if value.len() < 2 || !value.as_bytes()[0].is_ascii_digit() || value.as_bytes()[0] == b'0' {
        return Err(format!("invalid memory size {value:?}"));
    }
    let (amount, suffix) = value.split_at(value.len() - 1);
    if !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid memory size {value:?}"));
    }
    let multiplier = match suffix {
        "K" => 1u64 << 10,
        "M" => 1u64 << 20,
        "G" => 1u64 << 30,
        _ => return Err(format!("invalid memory size {value:?}")),
    };
    let amount: u64 = amount
        .parse()
        .map_err(|_| format!("invalid memory size {value:?}"))?;
    amount
        .checked_mul(multiplier)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("invalid memory size {value:?}"))
}

pub fn current_status() -> Result<MemoryBudgetStatus, String> {
    Ok(MemoryBudgetStatus {
        total_bytes: total_budget()?,
        used_bytes: used_budget()?,
    })
}

pub fn validate(id: &str, manifest: &Manifest) -> Result<(), String> {
    if manifest.memory_budget.is_empty() {
        return Ok(());
    }
    let required = parse_memory_size(&manifest.memory_budget)?;
    let status = current_status()?;
    let available = status.total_bytes.saturating_sub(status.used_bytes);
    if required > available {
        return Err(format!(
            "extension {id} memory budget is insufficient: required={required} available={available} used={} total={}",
            status.used_bytes, status.total_bytes
        ));
    }
    Ok(())
}

fn total_budget() -> Result<u64, String> {
    if let Ok(data) = fs::read(paths::MACHINE_PROFILE) {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data) {
            if let Some(budget) = value
                .get("extension_memory_budget")
                .and_then(|item| item.as_str())
            {
                if !budget.is_empty() {
                    return parse_memory_size(budget)
                        .map_err(|err| format!("invalid machine extension_memory_budget: {err}"));
                }
            }
        }
    }
    let file = fs::File::open("/proc/meminfo").map_err(|_| "read system memory".to_string())?;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|_| "read system memory".to_string())?;
        let mut fields = line.split_whitespace();
        if fields.next() != Some("MemTotal:") {
            continue;
        }
        let kilobytes: u64 = fields
            .next()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| "system memory total is unavailable".to_string())?;
        return kilobytes
            .checked_mul(1024)
            .map(|bytes| bytes / 2)
            .ok_or_else(|| "system memory total is unavailable".into());
    }
    Err("system memory total is unavailable".into())
}

fn used_budget() -> Result<u64, String> {
    let entries = match fs::read_dir(paths::APPS_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.to_string()),
    };
    let mut used = 0u64;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || !paths::valid_id(&id) {
            continue;
        }
        let Ok(state) = read_state(&id) else {
            continue;
        };
        if !state.enabled {
            continue;
        }
        let Ok(extension) = load_installed(&id, &state.active_version) else {
            continue;
        };
        if extension.manifest.memory_budget.is_empty() {
            continue;
        }
        let budget = parse_memory_size(&extension.manifest.memory_budget)?;
        used = used
            .checked_add(budget)
            .ok_or_else(|| "enabled extension memory budget overflow".to_string())?;
    }
    Ok(used)
}

#[cfg(test)]
mod tests {
    use super::parse_memory_size;

    #[test]
    fn parses_megabytes() {
        assert_eq!(parse_memory_size("32M").unwrap(), 32 << 20);
        assert!(parse_memory_size("0M").is_err());
        assert!(parse_memory_size("32").is_err());
    }
}
