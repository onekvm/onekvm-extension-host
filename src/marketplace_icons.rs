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

/// Extension icons are shown at most about 28 px wide, so a package icon may
/// hold at most 16 KiB, and a PNG or WebP icon at most 128 px on each side.
pub const MAX_ICON_BYTES: usize = 16 * 1024;
pub const MAX_ICON_PIXELS: u32 = 128;

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
    if !icon_fits(mime, &bytes) {
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

/// Whether an icon stays within the size limits; oversized icons are dropped
/// and the default icon shows instead.
pub fn icon_fits(mime: &str, bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > MAX_ICON_BYTES {
        return false;
    }
    match mime {
        "image/svg+xml" => true,
        "image/png" | "image/webp" => raster_size(bytes).is_some_and(|(width, height)| {
            (1..=MAX_ICON_PIXELS).contains(&width) && (1..=MAX_ICON_PIXELS).contains(&height)
        }),
        _ => false,
    }
}

/// Reads the pixel size from a PNG or WebP header.
fn raster_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let u32_be = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let u16_le =
        |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32);
    let u24_le = |at: usize| {
        let part = bytes.get(at..at + 3)?;
        Some(u32::from(part[0]) | u32::from(part[1]) << 8 | u32::from(part[2]) << 16)
    };
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR") {
        return Some((u32_be(16)?, u32_be(20)?));
    }
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return None;
    }
    match bytes.get(12..16)? {
        b"VP8 " => Some((u16_le(26)? & 0x3fff, u16_le(28)? & 0x3fff)),
        b"VP8L" => {
            let bits = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((u24_le(24)? + 1, u24_le(27)? + 1)),
        _ => None,
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
        let root =
            std::env::temp_dir().join(format!("onekvm-marketplace-icons-{}", std::process::id()));
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

        let png = store(&root, &png_data_url(16, 16)).expect("png");
        fs::write(root.join("notes.txt"), b"kept").unwrap();
        prune(&root, &BTreeSet::from([png.clone()]));
        assert!(!exists(&root, &name));
        assert!(exists(&root, &png));
        assert!(root.join("notes.txt").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn png_data_url(width: u32, height: u32) -> String {
        format!(
            "data:image/png;base64,{}",
            crate::ipk::base64_encode(&png_header(width, height))
        )
    }

    #[test]
    fn icons_over_the_size_limits_are_refused() {
        assert!(icon_fits("image/png", &png_header(128, 96)));
        assert!(!icon_fits("image/png", &png_header(129, 64)));
        assert!(!icon_fits("image/png", &png_header(0, 64)));
        assert!(!icon_fits("image/png", b"not a png"));

        let mut large = png_header(64, 64);
        large.resize(MAX_ICON_BYTES + 1, 0);
        assert!(!icon_fits("image/png", &large));
        assert!(icon_fits("image/svg+xml", b"<svg/>"));
        assert!(!icon_fits("image/svg+xml", &vec![b' '; MAX_ICON_BYTES + 1]));

        let mut lossless = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2f".to_vec();
        lossless.extend_from_slice(&(95u32 | 63 << 14).to_le_bytes());
        assert_eq!(raster_size(&lossless), Some((96, 64)));
        let mut extended = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0".to_vec();
        extended.extend_from_slice(&[199, 0, 0, 127, 0, 0]);
        assert_eq!(raster_size(&extended), Some((200, 128)));
        assert!(!icon_fits("image/webp", &extended));
    }
}
