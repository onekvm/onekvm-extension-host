use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::paths;

/// opkg downloads through GNU wget, which by default waits 15 minutes on a
/// silent server and tries 20 times. Package downloads give up after about 20
/// seconds instead, so the next mirror can take over.
const OPKG_WGETRC: &str = "/run/onekvm/opkg-wgetrc";
const OPKG_WGETRC_TEXT: &str =
    "# Written by onekvm-extension-host for package downloads.\ntimeout = 10\ntries = 2\nwaitretry = 1\n";
/// One package source's lists, including a slow but working link.
const OPKG_SOURCE_UPDATE_TIMEOUT: Duration = Duration::from_secs(60);

/// Points opkg's wget at the package download time limits.
pub fn apply_opkg_download_limits(command: &mut Command) {
    let path = Path::new(OPKG_WGETRC);
    if fs::read(path).ok().as_deref() != Some(OPKG_WGETRC_TEXT.as_bytes()) {
        let written = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .map_err(|e| e.to_string())
            .and_then(|_| atomic_write(path, OPKG_WGETRC_TEXT.as_bytes(), 0o644));
        if written.is_err() {
            return;
        }
    }
    command.env("WGETRC", path);
}

/// A child with the system proxy, and for opkg the download time limits.
fn child_command(program: &str) -> Result<Command, String> {
    let mut command = Command::new(program);
    crate::proxy::apply(&mut command)?;
    if program == paths::OPKG {
        apply_opkg_download_limits(&mut command);
    }
    Ok(command)
}

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
    let mut child = child_command(command)?;
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
    let mut child = child_command(command)?;
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
    let mut child = child_command(command)?;
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

/// Updates the package lists named in one opkg configuration file.
pub fn opkg_update_with(config: &Path) -> Result<(), String> {
    fs::create_dir_all(paths::OPKG_TMP)
        .map_err(|e| format!("prepare opkg temporary directory: {e}"))?;
    let args = vec![
        "-f".to_owned(),
        config.to_string_lossy().into_owned(),
        "--tmp-dir".to_owned(),
        paths::OPKG_TMP.to_owned(),
        "update".to_owned(),
    ];
    let finished = run_captured(paths::OPKG, &args, OPKG_SOURCE_UPDATE_TIMEOUT)?;
    let detail = error_lines(&finished.output);
    match finished.code {
        Some(0) => Ok(()),
        Some(code) => Err(format!("opkg update failed with {code}: {detail}")),
        None => Err(format!(
            "opkg update timed out after {} seconds: {detail}",
            OPKG_SOURCE_UPDATE_TIMEOUT.as_secs()
        )),
    }
}

/// A finished child: its exit code, or `None` when the deadline stopped it,
/// and its combined output.
pub struct Captured {
    pub code: Option<i32>,
    pub output: String,
}

/// Runs a command in its own process group with a deadline and keeps its
/// stdout and stderr. Only a failure to start it is an error.
pub fn run_captured(command: &str, args: &[String], timeout: Duration) -> Result<Captured, String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = child_command(command)?;
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{command}: {e}"))?;
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = pipe.read_to_end(&mut text);
            text
        })
    })
    .collect();
    let deadline = std::time::Instant::now() + timeout;
    let code = loop {
        match child.try_wait().map_err(|e| format!("{command}: {e}"))? {
            Some(status) => break Some(status.code().unwrap_or(-1)),
            None if std::time::Instant::now() >= deadline => {
                let pid = child.id() as libc::pid_t;
                if pid > 0 {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut output = Vec::new();
    for reader in readers {
        output.extend(reader.join().unwrap_or_default());
    }
    Ok(Captured {
        code,
        output: String::from_utf8_lossy(&output).into_owned(),
    })
}

/// The error lines of opkg output, or its last line.
pub fn error_lines(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let errors: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.to_ascii_lowercase().contains("error") || line.contains("Failed"))
        .collect();
    let mut text = if errors.is_empty() {
        lines.last().copied().unwrap_or_default().to_owned()
    } else {
        errors.join("; ")
    };
    if text.len() > 600 {
        let mut end = 600;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
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
    let mut child = child_command(paths::OPKG)?;
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
    use super::{error_lines, run_captured, run_with_timeout};

    #[test]
    fn captured_commands_keep_output_and_stop_at_the_deadline() {
        let done = run_captured(
            "/bin/sh",
            &["-c".into(), "echo out; echo err >&2; exit 3".into()],
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(done.code, Some(3));
        assert!(done.output.contains("out") && done.output.contains("err"));
        let stopped = run_captured(
            "/bin/sh",
            &["-c".into(), "echo started; sleep 10".into()],
            std::time::Duration::from_millis(200),
        )
        .unwrap();
        assert_eq!(stopped.code, None);
        assert!(stopped.output.contains("started"));
    }

    #[test]
    fn opkg_errors_are_kept_from_output() {
        let output = "Downloading https://a/feed/Packages.\nerror: opkg_download_backend: Failed to download https://a/feed/Packages, wget returned 8.\n";
        assert_eq!(
            error_lines(output),
            "error: opkg_download_backend: Failed to download https://a/feed/Packages, wget returned 8."
        );
        assert_eq!(error_lines("Updated source 'x'.\n"), "Updated source 'x'.");
    }

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
