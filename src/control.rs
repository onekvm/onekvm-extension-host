use std::collections::BTreeMap;

pub fn parse_control(data: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(data).map_err(|_| "control file is not UTF-8".to_string())?;
    let mut fields = BTreeMap::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if current.is_empty() {
                return Err("invalid control continuation".into());
            }
            fields
                .entry(current.clone())
                .and_modify(|value: &mut String| {
                    value.push('\n');
                    value.push_str(line.trim());
                });
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("invalid control line {line:?}"));
        };
        current = key.to_owned();
        fields.insert(current.clone(), value.trim().to_owned());
    }
    for required in ["Package", "Version", "Architecture"] {
        if fields.get(required).map(String::as_str).unwrap_or_default().is_empty() {
            return Err(format!("control field {required} is missing"));
        }
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::parse_control;

    #[test]
    fn parses_required_fields() {
        let fields = parse_control(b"Package: onekvm-extension-edid\nVersion: 1.0.0\nArchitecture: riscv64\n").unwrap();
        assert_eq!(fields["Package"], "onekvm-extension-edid");
        assert_eq!(fields["Version"], "1.0.0");
    }

    #[test]
    fn rejects_missing_package() {
        assert!(parse_control(b"Version: 1\nArchitecture: all\n").is_err());
    }
}
