//! Public keys explicitly trusted by the administrator when saving a source.
//! opkg uses a shared keyring; rebuild its runtime copy from image keys and
//! enabled sources so deleting a source never leaves a permanent trust anchor.
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::sys;

pub const MAX_PUBLIC_KEY_BYTES: usize = 64 * 1024;
const KEY_PREFIX: &str = "# OneKVM-Source-Public-Key:";
const FINGERPRINT_PREFIX: &str = "# OneKVM-Source-Fingerprint:";
const BEGIN: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
const END: &str = "-----END PGP PUBLIC KEY BLOCK-----";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceKey {
    pub public_key: String,
    pub fingerprint: String,
}

pub fn is_key_header(line: &str) -> bool {
    let line = line.trim();
    line.starts_with(KEY_PREFIX) || line.starts_with(FINGERPRINT_PREFIX)
}

pub fn read_headers(text: &str) -> SourceKey {
    let mut key = SourceKey::default();
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix(KEY_PREFIX) {
            key.public_key.push_str(value.trim());
            key.public_key.push('\n');
        } else if let Some(value) = line.trim().strip_prefix(FINGERPRINT_PREFIX) {
            key.fingerprint = value.trim().into();
        }
    }
    key
}

pub fn encode_headers(key: &SourceKey) -> String {
    if key.public_key.is_empty() {
        return String::new();
    }
    let mut out = format!("{FINGERPRINT_PREFIX} {}\n", key.fingerprint);
    for line in key.public_key.lines() {
        out.push_str(&format!("{KEY_PREFIX} {line}\n"));
    }
    out
}

pub fn root_owned_source(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|info| info.file_type().is_file() && info.uid() == 0 && info.mode() & 0o022 == 0)
}

pub fn validate(public_key: &str) -> Result<SourceKey, String> {
    let text = public_key.trim();
    if text.len() > MAX_PUBLIC_KEY_BYTES
        || !text.starts_with(BEGIN)
        || !text.ends_with(END)
        || text.matches(BEGIN).count() != 1
        || text.matches(END).count() != 1
        || text.contains("PRIVATE KEY")
        || text.contains("SECRET KEY")
    {
        return Err("provide one ASCII-armored OpenPGP public key (maximum 64 KiB)".into());
    }
    let home = TempHome::new(&std::env::temp_dir())?;
    let input = home.0.join("source.asc");
    fs::write(&input, text).map_err(|e| format!("stage source public key: {e}"))?;
    let output = gpg(&home.0)
        .args([
            "--with-colons",
            "--import-options",
            "show-only",
            "--dry-run",
            "--import",
        ])
        .arg(input)
        .output()
        .map_err(|e| format!("inspect source public key: {e}"))?;
    if !output.status.success() {
        return Err("invalid OpenPGP public key".into());
    }
    let fingerprint = parse_fingerprint(&String::from_utf8_lossy(&output.stdout))?;
    Ok(SourceKey {
        public_key: format!("{text}\n"),
        fingerprint,
    })
}

fn parse_fingerprint(listing: &str) -> Result<String, String> {
    let mut primary = 0;
    let mut fingerprint = None;
    let mut want_fingerprint = false;
    for line in listing.lines() {
        let fields: Vec<_> = line.split(':').collect();
        match fields.first().copied() {
            Some("sec" | "ssb") => return Err("private keys are not accepted".into()),
            Some("pub") => {
                primary += 1;
                want_fingerprint = true;
                if matches!(fields.get(1).copied(), Some("r" | "e" | "d"))
                    || !fields.get(11).is_some_and(|caps| caps.contains(['s', 'S']))
                {
                    return Err("public key must be valid and support signing".into());
                }
            }
            Some("fpr") if want_fingerprint => {
                fingerprint = fields.get(9).map(|s| s.to_string());
                want_fingerprint = false;
            }
            _ => {}
        }
    }
    match fingerprint {
        Some(value)
            if primary == 1
                && matches!(value.len(), 40 | 64)
                && value.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            Ok(value.to_ascii_uppercase())
        }
        _ => Err("provide exactly one OpenPGP public key".into()),
    }
}

fn gpg(home: &Path) -> Command {
    let mut command = Command::new("/usr/bin/gpg");
    command
        .args(["--no-options", "--batch", "--no-tty", "--no-autostart"])
        .arg("--homedir")
        .arg(home);
    command
}

pub fn prepare_keyring(keys: &[SourceKey]) -> Result<(), String> {
    prepare_keyring_at(
        Path::new("/etc/opkg/gpg"),
        Path::new("/run/onekvm/opkg-gpg"),
        keys,
    )
}

fn prepare_keyring_at(image: &Path, runtime: &Path, keys: &[SourceKey]) -> Result<(), String> {
    let parent = runtime.parent().ok_or("invalid opkg keyring path")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let home = TempHome::new(parent)?;
    // Start from the image on every refresh, never from a ring that previously
    // contained third-party keys. Shared keys remain while any source uses them.
    for name in ["pubring.kbx", "trustdb.gpg"] {
        fs::copy(image.join(name), home.0.join(name))
            .map_err(|e| format!("copy image opkg keyring: {e}"))?;
    }
    for (index, key) in keys.iter().enumerate() {
        let input = home.0.join(format!("source-{index}.asc"));
        fs::write(&input, &key.public_key).map_err(|e| e.to_string())?;
        let output = gpg(&home.0)
            .args(["--import-options", "import-minimal", "--import"])
            .arg(input)
            .output()
            .map_err(|e| format!("import source public key: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "cannot import source public key {}",
                key.fingerprint
            ));
        }
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(runtime)
        .map_err(|e| format!("prepare runtime opkg keyring: {e}"))?;
    for name in ["pubring.kbx", "trustdb.gpg"] {
        let data = fs::read(home.0.join(name)).map_err(|e| e.to_string())?;
        sys::atomic_write(&runtime.join(name), &data, 0o600)?;
    }
    Ok(())
}

struct TempHome(PathBuf);

impl TempHome {
    fn new(parent: &Path) -> Result<Self, String> {
        for number in 0..1000 {
            let path = parent.join(format!("onekvm-gpg-{}-{number}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("prepare public key inspection: {err}")),
            }
        }
        Err("cannot create temporary GnuPG home".into())
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate(home: &Path, uid: &str) -> SourceKey {
        let output = Command::new("/usr/bin/gpg")
            .args([
                "--no-options",
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
            ])
            .arg("--homedir")
            .arg(home)
            .args(["--quick-generate-key", uid, "ed25519", "sign", "0"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = gpg(home)
            .args(["--armor", "--export", uid])
            .output()
            .unwrap();
        validate(&String::from_utf8(output.stdout).unwrap()).unwrap()
    }

    fn kill_agent(home: &Path) {
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(home)
            .args(["--kill", "all"])
            .output();
    }

    #[test]
    fn rejects_invalid_multiple_expired_and_secret_keys() {
        assert!(validate("not a key").is_err());
        assert!(validate(&format!(
            "{BEGIN}\n{}\n{END}",
            "A".repeat(MAX_PUBLIC_KEY_BYTES)
        ))
        .is_err());
        let home = TempHome::new(&std::env::temp_dir()).unwrap();
        let key = generate(&home.0, "OneKVM key test <key-test@invalid>");
        assert_eq!(read_headers(&encode_headers(&key)), key);
        assert!(validate(&format!("{}{}", key.public_key, key.public_key)).is_err());
        let secret = gpg(&home.0)
            .args(["--armor", "--export-secret-keys"])
            .output()
            .unwrap();
        let secret = String::from_utf8(secret.stdout).unwrap();
        assert!(!secret.is_empty());
        assert!(validate(&secret).is_err());
        // Secret packets must also be rejected when disguised with public armor.
        assert!(validate(&secret.replace("PRIVATE KEY", "PUBLIC KEY")).is_err());
        let fingerprint = "A".repeat(40);
        let listing = format!("pub:e:255:22:key::::::scSC:\nfpr:::::::::{fingerprint}:\n");
        assert!(parse_fingerprint(&listing).is_err());
        kill_agent(&home.0);
    }

    #[test]
    fn runtime_ring_accepts_signatures_and_drops_removed_source_keys() {
        let root = TempHome::new(&std::env::temp_dir()).unwrap();
        let image = root.0.join("image");
        let external = root.0.join("external");
        let runtime = root.0.join("runtime");
        fs::DirBuilder::new().mode(0o700).create(&image).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&external).unwrap();
        let official = generate(&image, "OneKVM image test <image-test@invalid>");
        let custom = generate(&external, "OneKVM external test <external-test@invalid>");
        let data = root.0.join("Packages");
        fs::write(&data, "Package: onekvm-extension-test\nVersion: 1.0\n").unwrap();
        let signature = root.0.join("Packages.asc");
        let signed = gpg(&external)
            .args(["--armor", "--detach-sign", "--output"])
            .arg(&signature)
            .arg(&data)
            .output()
            .unwrap();
        assert!(signed.status.success());
        prepare_keyring_at(&image, &runtime, &[custom.clone(), custom.clone()]).unwrap();
        let verify = || {
            gpg(&runtime)
                .arg("--verify")
                .arg(&signature)
                .arg(&data)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(
            verify(),
            "imported public key must verify an external signed index"
        );
        fs::write(&data, "Package: tampered\n").unwrap();
        assert!(!verify(), "modified indexes must fail verification");
        fs::write(&data, "Package: onekvm-extension-test\nVersion: 1.0\n").unwrap();
        prepare_keyring_at(&image, &runtime, &[]).unwrap();
        assert!(
            !verify(),
            "removed or disabled source key must no longer verify"
        );
        let keys = gpg(&runtime)
            .args(["--with-colons", "--list-keys"])
            .output()
            .unwrap();
        let keys = String::from_utf8_lossy(&keys.stdout);
        assert!(keys.contains(&official.fingerprint));
        assert!(!keys.contains(&custom.fingerprint));
        // Recreating the runtime ring models a reboot; configured keys return.
        fs::remove_dir_all(&runtime).unwrap();
        prepare_keyring_at(&image, &runtime, &[custom]).unwrap();
        assert!(verify());
        kill_agent(&image);
        kill_agent(&external);
    }

    #[test]
    fn writable_or_symlinked_sources_cannot_grant_trust() {
        use std::os::unix::fs::PermissionsExt;
        let home = TempHome::new(&std::env::temp_dir()).unwrap();
        let path = home.0.join("source.conf");
        fs::write(&path, "src test https://example.org/test\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!root_owned_source(&path));
        let link = home.0.join("link.conf");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(!root_owned_source(&link));
    }
}
