//! Plugin marketplace icons kept as files.
//!
//! Package indexes carry icons as data URLs. The marketplace list stores each
//! icon once under its content hash, and Core serves it at
//! `/api/extension-icons/<name>`, so browsers can keep it instead of receiving
//! every icon again with each list.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

pub const URL_PREFIX: &str = "/api/extension-icons/";

const TYPES: &[(&str, &str)] = &[
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("webp", "image/webp"),
];

/// Stores a `data:image/...;base64,` icon and returns its file name.
pub fn store(root: &Path, data_url: &str) -> Option<String> {
    let rest = data_url.strip_prefix("data:")?;
    let (mime, encoded) = rest.split_once(";base64,")?;
    let (extension, _) = TYPES.iter().find(|(_, known)| *known == mime)?;
    let bytes = crate::feed::decode_base64(encoded).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let digest = Sha256::digest(&bytes);
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let name = format!("{hash}.{extension}");
    let path = root.join(&name);
    if !path.is_file() {
        crate::sys::atomic_write(&path, &bytes, 0o644).ok()?;
    }
    Some(name)
}

/// The file name behind an icon URL from the marketplace list.
pub fn name_from_url(url: &str) -> Option<&str> {
    url.strip_prefix(URL_PREFIX).filter(|name| valid_name(name))
}

pub fn exists(root: &Path, name: &str) -> bool {
    valid_name(name) && root.join(name).is_file()
}

/// Reads a stored icon back as a data URL for callers that expect one inline.
pub fn data_url(root: &Path, name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    let (_, extension) = name.rsplit_once('.')?;
    let (_, mime) = TYPES.iter().find(|(known, _)| *known == extension)?;
    let bytes = fs::read(root.join(name)).ok()?;
    Some(format!(
        "data:{mime};base64,{}",
        crate::ipk::base64_encode(&bytes)
    ))
}

/// Removes stored icons that the current list no longer uses.
pub fn prune(root: &Path, keep: &BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if valid_name(&name) && !keep.contains(&name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn valid_name(name: &str) -> bool {
    let Some((hash, extension)) = name.split_once('.') else {
        return false;
    };
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && TYPES.iter().any(|(known, _)| *known == extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_stored_once_by_content_and_read_back() {
        let root = std::env::temp_dir().join(format!(
            "onekvm-marketplace-icons-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let svg = "data:image/svg+xml;base64,PHN2Zy8+";
        let name = store(&root, svg).expect("stored");
        assert!(name.ends_with(".svg"));
        assert_eq!(fs::read(root.join(&name)).unwrap(), b"<svg/>");
        assert_eq!(store(&root, svg).as_deref(), Some(name.as_str()));
        assert_eq!(data_url(&root, &name).as_deref(), Some(svg));
        assert_eq!(
            name_from_url(&format!("{URL_PREFIX}{name}")),
            Some(name.as_str())
        );
        assert!(store(&root, "data:text/html;base64,PHN2Zy8+").is_none());
        assert!(store(&root, "data:image/png;base64,").is_none());
        assert!(name_from_url("/api/extension-icons/../secret.png").is_none());

        let png = store(&root, "data:image/png;base64,iVBORw0KGgo=").expect("png");
        fs::write(root.join("notes.txt"), b"kept").unwrap();
        prune(&root, &BTreeSet::from([png.clone()]));
        assert!(!exists(&root, &name));
        assert!(exists(&root, &png));
        assert!(root.join("notes.txt").is_file());
        let _ = fs::remove_dir_all(&root);
    }
}
