use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::paths;

pub fn with_lock<T, F: FnOnce() -> Result<T, String>>(fn_: F) -> Result<T, String> {
    if let Some(parent) = Path::new(paths::LOCK_PATH).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(paths::LOCK_PATH)
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("lock extension manager".into());
    }
    let result = fn_();
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    result
}

pub fn runtime_ids() -> Result<(u32, u32), String> {
    let file = File::open("/etc/passwd").map_err(|e| e.to_string())?;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let mut fields = line.split(':');
        let name = fields.next().unwrap_or_default();
        if name != "onekvm-extension" {
            continue;
        }
        let _ = fields.next();
        let uid = fields
            .next()
            .ok_or_else(|| "invalid runtime account onekvm-extension".to_string())?
            .parse()
            .map_err(|_| "invalid runtime account onekvm-extension".to_string())?;
        let gid = fields
            .next()
            .ok_or_else(|| "invalid runtime account onekvm-extension".to_string())?
            .parse()
            .map_err(|_| "invalid runtime account onekvm-extension".to_string())?;
        return Ok((uid, gid));
    }
    Err("runtime account onekvm-extension does not exist".into())
}

pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "invalid write path".to_string())?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut temporary = tempfile_in(dir)?;
    let tmp_path = temporary.path().clone();
    temporary
        .file
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| e.to_string())?;
    temporary.file.write_all(data).map_err(|e| e.to_string())?;
    temporary.file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp_path, path).map_err(|e| e.to_string())?;
    File::open(dir).and_then(|file| file.sync_all()).map_err(|e| e.to_string())
}

struct Temp {
    path: std::path::PathBuf,
    file: File,
}

impl Temp {
    fn path(&self) -> &std::path::PathBuf {
        &self.path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn tempfile_in(dir: &Path) -> Result<Temp, String> {
    for n in 0..1000 {
        let path = dir.join(format!(".tmp-{}-{}", std::process::id(), n));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok(Temp { path, file }),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        }
    }
    Err("create temporary file".into())
}

pub fn run(command: &str, args: &[String]) -> Result<(), String> {
    let status = std::process::Command::new(command)
        .args(args)
        .status()
        .map_err(|e| format!("{command}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{command} failed with {}",
            status.code().unwrap_or(-1)
        ))
    }
}

pub fn quiet(command: &str, args: &[&str]) -> bool {
    std::process::Command::new(command)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn output(command: &str, args: &[String]) -> Result<Vec<u8>, String> {
    let result = std::process::Command::new(command)
        .args(args)
        .output()
        .map_err(|e| format!("{command}: {e}"))?;
    if result.status.success() {
        Ok(result.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr);
        Err(format!(
            "{}: {}",
            Path::new(command)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or(command),
            stderr.trim()
        ))
    }
}

pub fn opkg(args: &[&str]) -> Result<(), String> {
    fs::create_dir_all(paths::OPKG_TMP)
        .map_err(|e| format!("prepare opkg temporary directory: {e}"))?;
    let mut owned = vec!["--tmp-dir".to_owned(), paths::OPKG_TMP.to_owned()];
    owned.extend(args.iter().map(|value| (*value).to_owned()));
    run(paths::OPKG, &owned)
}

pub fn opkg_output(args: &[&str]) -> Result<Vec<u8>, String> {
    fs::create_dir_all(paths::OPKG_TMP)
        .map_err(|e| format!("prepare opkg temporary directory: {e}"))?;
    let mut owned = vec!["--tmp-dir".to_owned(), paths::OPKG_TMP.to_owned()];
    owned.extend(args.iter().map(|value| (*value).to_owned()));
    output(paths::OPKG, &owned)
}

pub fn opkg_in(dir: &Path, args: &[&str]) -> Result<(), String> {
    fs::create_dir_all(paths::OPKG_TMP)
        .map_err(|e| format!("prepare opkg temporary directory: {e}"))?;
    let mut owned = vec!["--tmp-dir".to_owned(), paths::OPKG_TMP.to_owned()];
    owned.extend(args.iter().map(|value| (*value).to_owned()));
    let status = std::process::Command::new(paths::OPKG)
        .args(&owned)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("opkg: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("opkg failed with {}", status.code().unwrap_or(-1)))
    }
}
