use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

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

/// Run a manager operation only when the global manager lock is immediately
/// available. Feed refreshes use this path so a slow network operation cannot
/// make every other extension request wait behind the flock indefinitely.
pub fn try_with_lock<T, F: FnOnce() -> Result<T, String>>(fn_: F) -> Result<T, String> {
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
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK)
        {
            return Err("extension manager busy".into());
        }
        return Err(format!("lock extension manager: {error}"));
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
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
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
    let mut child = std::process::Command::new(command);
    crate::proxy::apply(&mut child)?;
    let status = child
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

/// Run a command in its own process group and terminate the whole group when
/// the deadline expires. opkg launches wget/openssl children, so killing only
/// the direct child would leave the network operation behind.
pub fn run_with_timeout(command: &str, args: &[String], timeout: Duration) -> Result<(), String> {
    let mut child = Command::new(command);
    crate::proxy::apply(&mut child)?;
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        child.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child
        .args(args)
        .spawn()
        .map_err(|e| format!("{command}: {e}"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| format!("{command}: {e}"))? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                return Err(format!(
                    "{command} failed with {}",
                    status.code().unwrap_or(-1)
                ));
            }
            None if std::time::Instant::now() >= deadline => {
                let pid = child.id() as libc::pid_t;
                if pid > 0 {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{command} timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
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
    let mut child = std::process::Command::new(command);
    crate::proxy::apply(&mut child)?;
    let result = child
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

pub fn opkg_update() -> Result<(), String> {
    fs::create_dir_all(paths::OPKG_TMP)
        .map_err(|e| format!("prepare opkg temporary directory: {e}"))?;
    let args = vec![
        "--tmp-dir".to_owned(),
        paths::OPKG_TMP.to_owned(),
        "update".to_owned(),
    ];
    run_with_timeout(paths::OPKG, &args, Duration::from_secs(20))
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
    let mut child = std::process::Command::new(paths::OPKG);
    crate::proxy::apply(&mut child)?;
    let status = child
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

#[cfg(test)]
mod tests {
    use super::run_with_timeout;

    #[test]
    fn timeout_terminates_command() {
        let error = run_with_timeout(
            "/bin/sh",
            &["-c".into(), "sleep 10".into()],
            std::time::Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.contains("timed out"));
    }
}
