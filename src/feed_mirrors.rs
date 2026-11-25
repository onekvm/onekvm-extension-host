//! The host selects the distribution feed; native opkg follows the same symlink.
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{paths, sys};

/// Why a package source could not be used, ordered by how well each explains
/// a failure when several servers failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// HTTP 404 or 410: nothing is published at this address. For the
    /// distribution feed, this system version has no feed yet.
    NotFound,
    /// The lists failed signature or validity checks.
    Rejected,
    /// TLS certificate verification failed.
    Certificate,
    /// The server answered with another HTTP error.
    ServerError,
    /// Timeout, DNS or connection failure.
    Unreachable,
    Failed,
}

impl Failure {
    fn describe(self) -> &'static str {
        match self {
            Self::NotFound => "not published (HTTP 404)",
            Self::Rejected => "rejected by signature or validity checks",
            Self::Certificate => "certificate verification failed",
            Self::ServerError => "server error",
            Self::Unreachable => "timed out or unreachable",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchError {
    pub failure: Failure,
    pub detail: String,
}

impl FetchError {
    pub fn new(failure: Failure, detail: impl Into<String>) -> Self {
        Self {
            failure,
            detail: detail.into(),
        }
    }

    /// The failure that best explains several, with every detail kept.
    fn combine(errors: Vec<FetchError>) -> FetchError {
        let failure = errors
            .iter()
            .map(|error| error.failure)
            .min()
            .unwrap_or(Failure::Failed);
        let detail = errors
            .into_iter()
            .map(|error| error.detail)
            .collect::<Vec<_>>()
            .join("; ");
        FetchError::new(failure, detail)
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Updates the distribution feed lists from the default server, or from the
/// fastest mirror that works. Returns the server used.
pub fn refresh(update: impl FnMut() -> Result<(), FetchError>) -> Result<String, FetchError> {
    refresh_at(
        Path::new(paths::FEED_CONFIG),
        Path::new(paths::FEED_DEFAULT),
        Path::new(paths::FEED_PROFILES),
        Path::new(paths::FEED_MIRRORS),
        probe,
        update,
    )
}

/// Runs a package download; when the active distribution server fails to
/// deliver a package, moves to the fastest other mirror and downloads again.
/// A mirror that works stays selected for later downloads.
pub fn download(attempt: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    download_at(
        Path::new(paths::FEED_CONFIG),
        Path::new(paths::FEED_DEFAULT),
        Path::new(paths::FEED_PROFILES),
        Path::new(paths::FEED_MIRRORS),
        probe,
        attempt,
    )
}

/// Whether an opkg error came from downloading a file.
pub fn is_download_failure(error: &str) -> bool {
    error.contains("Failed to download") || error.contains("wget returned")
}

/// Explains a failed opkg update, asking the server again for the file that
/// failed when the output does not say why.
pub fn update_failure(error: &str) -> FetchError {
    update_failure_with(error, probe)
}

fn update_failure_with(
    error: &str,
    probe: impl Fn(&str) -> Result<Duration, Failure>,
) -> FetchError {
    let failure = if let Some(url) = failed_download_url(error) {
        match probe(url) {
            Err(failure) => failure,
            Ok(_) => match wget_exit_code(error) {
                Some(4) => Failure::Unreachable,
                Some(5) => Failure::Certificate,
                Some(8) => Failure::ServerError,
                _ => Failure::Failed,
            },
        }
    } else if error.contains("timed out") {
        Failure::Unreachable
    } else if error.to_ascii_lowercase().contains("signature") {
        Failure::Rejected
    } else {
        Failure::Failed
    };
    FetchError::new(failure, error)
}

fn failed_download_url(error: &str) -> Option<&str> {
    let rest = &error[error.find("Failed to download ")? + "Failed to download ".len()..];
    let end = rest.find([',', ' ']).unwrap_or(rest.len());
    let url = &rest[..end];
    (url.starts_with("https://") || url.starts_with("http://")).then_some(url)
}

fn wget_exit_code(error: &str) -> Option<i32> {
    let rest = &error[error.find("wget returned ")? + "wget returned ".len()..];
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Classifies a wget exit code and its `--server-response` output; `None`
/// means the deadline stopped it.
fn wget_failure(code: Option<i32>, output: &str) -> Failure {
    match code {
        Some(8) => match last_http_status(output) {
            Some(404 | 410) => Failure::NotFound,
            _ => Failure::ServerError,
        },
        None | Some(4) => Failure::Unreachable,
        Some(5) => Failure::Certificate,
        _ => Failure::Failed,
    }
}

fn last_http_status(output: &str) -> Option<u16> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            fields
                .next()
                .filter(|field| field.starts_with("HTTP/"))
                .and_then(|_| fields.next())
                .and_then(|code| code.parse().ok())
        })
        .last()
}

fn probe(url: &str) -> Result<Duration, Failure> {
    let started = Instant::now();
    let done = sys::run_captured(
        "/usr/bin/wget",
        &[
            "--quiet".into(),
            "--server-response".into(),
            "--tries=1".into(),
            "--timeout=3".into(),
            "--output-document=/dev/null".into(),
            "--".into(),
            url.into(),
        ],
        Duration::from_secs(4),
    )
    .map_err(|_| Failure::Failed)?;
    if done.code == Some(0) {
        Ok(started.elapsed())
    } else {
        Err(wget_failure(done.code, &done.output))
    }
}

struct Profile {
    text: String,
    probe_url: String,
    target: PathBuf,
}

fn feed_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

fn origin(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host_end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() || host.contains(['@', '?', '#']) {
        return None;
    }
    Some(&url[..8 + host_end])
}

fn profile_path(profiles: &Path, text: &str) -> PathBuf {
    profiles.join(format!("{:x}.conf", Sha256::digest(text.as_bytes())))
}

fn profiles(
    default: &str,
    defaults: &Path,
    directory: &Path,
    mirrors: &str,
) -> Option<Vec<Profile>> {
    let lines = feed_lines(default);
    let mut primary = None;
    let mut metadata = None;
    for line in &lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 3 || !matches!(fields[0], "src" | "src/gz") {
            return None;
        }
        let base = origin(fields[2])?;
        if primary.is_some_and(|current| current != base) {
            return None;
        }
        primary = Some(base);
        if fields[1] == "onekvm-metadata" {
            metadata = Some(fields[2]);
        }
    }
    let primary = primary?;
    let metadata = metadata?;
    let probe_url = format!("{}/Packages.asc", metadata.trim_end_matches('/'));
    let mut result = vec![Profile {
        text: default.into(),
        probe_url: probe_url.clone(),
        target: defaults.into(),
    }];
    for mirror in mirrors.lines().map(str::trim) {
        if mirror.is_empty() || mirror.starts_with('#') {
            continue;
        }
        let mirror = mirror.trim_end_matches('/');
        if origin(mirror) != Some(mirror) || mirror == primary {
            continue;
        }
        let mut text = String::new();
        for line in default.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if matches!(fields.first(), Some(&"src" | &"src/gz")) {
                text.push_str(&format!(
                    "{} {} {}{}\n",
                    fields[0],
                    fields[1],
                    mirror,
                    &fields[2][primary.len()..]
                ));
            } else {
                text.push_str(line);
                text.push('\n');
            }
        }
        result.push(Profile {
            target: profile_path(directory, &text),
            text,
            probe_url: probe_url.replacen(primary, mirror, 1),
        });
    }
    Some(result)
}

fn atomic_link(target: &Path, active: &Path) -> Result<(), String> {
    let directory = active.parent().ok_or("invalid feed config path")?;
    for n in 0..1000 {
        let temporary = directory.join(format!(".feed-link-{}-{n}", std::process::id()));
        match symlink(target, &temporary) {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("create feed symlink: {err}")),
            Ok(()) => {}
        }
        let result =
            fs::rename(&temporary, active).map_err(|e| format!("switch feed symlink: {e}"));
        let _ = fs::remove_file(&temporary);
        result?;
        return fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("sync feed symlink: {e}"));
    }
    Err("create temporary feed symlink".into())
}

/// The managed distribution feed: its current state, to put back after a
/// failed switch, and the default server followed by its mirrors.
struct Managed {
    current: String,
    mode: u32,
    old_target: Option<PathBuf>,
    candidates: Vec<Profile>,
}

/// `None` when the feed is disabled, customized or has no usable layout;
/// those configurations remain administrator owned.
fn managed(
    active: &Path,
    defaults: &Path,
    directory: &Path,
    mirrors: &Path,
) -> Result<Option<Managed>, String> {
    let (Ok(current), Ok(default), Ok(info)) = (
        fs::read_to_string(active),
        fs::read_to_string(defaults),
        fs::symlink_metadata(active),
    ) else {
        return Ok(None);
    };
    let old_target = fs::read_link(active).ok();
    let is_managed = old_target
        .as_ref()
        .is_some_and(|target| target == defaults || *target == profile_path(directory, &current));
    if !is_managed && feed_lines(&current) != feed_lines(&default) {
        return Ok(None);
    }
    let mirror_text = match fs::read_to_string(mirrors) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("read feed mirrors: {err}")),
    };
    Ok(
        profiles(&default, defaults, directory, &mirror_text).map(|candidates| Managed {
            current,
            mode: info.permissions().mode() & 0o777,
            old_target,
            candidates,
        }),
    )
}

impl Managed {
    /// The candidate opkg uses now; a regular file still holds the default.
    fn active_target(&self, defaults: &Path) -> PathBuf {
        self.old_target
            .clone()
            .unwrap_or_else(|| defaults.to_path_buf())
    }

    fn switch(&self, candidate: &Profile, defaults: &Path, active: &Path) -> Result<(), String> {
        if candidate.target != defaults {
            sys::atomic_write(&candidate.target, candidate.text.as_bytes(), 0o644)?;
        }
        atomic_link(&candidate.target, active)
    }

    fn restore(&self, active: &Path) -> Result<(), String> {
        match &self.old_target {
            Some(target) => atomic_link(target, active),
            None => sys::atomic_write(active, self.current.as_bytes(), self.mode),
        }
    }
}

/// Probes candidates concurrently and returns the reachable ones, fastest
/// first, with why the others are unreachable.
fn rank<'a>(
    candidates: &[&'a Profile],
    probe: &(impl Fn(&str) -> Result<Duration, Failure> + Sync),
) -> (Vec<&'a Profile>, Vec<FetchError>) {
    // HTTPS GET time includes DNS, TLS and response latency.
    let results = std::thread::scope(|scope| {
        let jobs: Vec<_> = candidates
            .iter()
            .map(|candidate| scope.spawn(move || (probe(&candidate.probe_url), *candidate)))
            .collect();
        jobs.into_iter()
            .filter_map(|job| job.join().ok())
            .collect::<Vec<_>>()
    });
    let mut reachable = Vec::new();
    let mut failures = Vec::new();
    for (result, candidate) in results {
        let server = origin(&candidate.probe_url).unwrap_or_default();
        match result {
            Ok(elapsed) => {
                eprintln!(
                    "Feed mirror {server} responded in {} ms",
                    elapsed.as_millis()
                );
                reachable.push((elapsed, candidate));
            }
            Err(failure) => failures.push(FetchError::new(
                failure,
                format!("{server}: {}", failure.describe()),
            )),
        }
    }
    reachable.sort_by_key(|(elapsed, _)| *elapsed);
    (
        reachable
            .into_iter()
            .map(|(_, candidate)| candidate)
            .collect(),
        failures,
    )
}

fn refresh_at(
    active: &Path,
    defaults: &Path,
    directory: &Path,
    mirrors: &Path,
    probe: impl Fn(&str) -> Result<Duration, Failure> + Sync,
    mut update: impl FnMut() -> Result<(), FetchError>,
) -> Result<String, FetchError> {
    let Some(state) = managed(active, defaults, directory, mirrors)
        .map_err(|e| FetchError::new(Failure::Failed, e))?
    else {
        return update().map(|_| String::new());
    };
    let mut switched = false;
    let mut failures = Vec::new();
    let mut attempt = |candidate: &Profile| -> Result<String, FetchError> {
        switched = true;
        state
            .switch(candidate, defaults, active)
            .map_err(|e| FetchError::new(Failure::Failed, e))?;
        let server = origin(&candidate.probe_url).unwrap_or_default().to_owned();
        eprintln!("Updating distribution feed from {server}");
        update()
            .map(|_| server.clone())
            .map_err(|error| FetchError::new(error.failure, format!("{server}: {}", error.detail)))
    };
    let primary = &state.candidates[0];
    let checked = if state.candidates.len() == 1 {
        Ok(Duration::ZERO)
    } else {
        probe(&primary.probe_url)
    };
    match checked {
        Ok(_) => match attempt(primary) {
            Ok(server) => return Ok(server),
            Err(error) => failures.push(error),
        },
        Err(failure) => failures.push(FetchError::new(
            failure,
            format!(
                "{}: {}",
                origin(&primary.probe_url).unwrap_or_default(),
                failure.describe()
            ),
        )),
    }
    let others: Vec<&Profile> = state.candidates[1..].iter().collect();
    let (reachable, unreachable) = rank(&others, &probe);
    failures.extend(unreachable);
    for candidate in reachable {
        match attempt(candidate) {
            Ok(server) => return Ok(server),
            Err(error) => failures.push(error),
        }
    }
    let mut error = FetchError::combine(failures);
    error.detail = format!("distribution feed refresh failed: {}", error.detail);
    if switched {
        if let Err(restore) = state.restore(active) {
            error.detail = format!("{}; restore feed configuration: {restore}", error.detail);
        }
    }
    Err(error)
}

fn download_at(
    active: &Path,
    defaults: &Path,
    directory: &Path,
    mirrors: &Path,
    probe: impl Fn(&str) -> Result<Duration, Failure> + Sync,
    mut attempt: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let first = match attempt() {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if !is_download_failure(&first) {
        return Err(first);
    }
    let Ok(Some(state)) = managed(active, defaults, directory, mirrors) else {
        return Err(first);
    };
    let current = state.active_target(defaults);
    let others: Vec<&Profile> = state
        .candidates
        .iter()
        .filter(|candidate| candidate.target != current)
        .collect();
    if others.is_empty() {
        return Err(first);
    }
    let (reachable, _) = rank(&others, &probe);
    let mut errors = vec![first];
    let mut switched = false;
    for candidate in reachable {
        let server = origin(&candidate.probe_url).unwrap_or_default().to_owned();
        if let Err(error) = state.switch(candidate, defaults, active) {
            errors.push(error);
            break;
        }
        switched = true;
        eprintln!("Downloading packages again from {server}");
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(format!("{server}: {error}")),
        }
    }
    let mut error = errors.join("; ");
    if switched {
        if let Err(restore) = state.restore(active) {
            error = format!("{error}; restore feed configuration: {restore}");
        }
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        active: PathBuf,
        defaults: PathBuf,
        directory: PathBuf,
        mirrors: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "onekvm-feed-mirrors-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(root.join("opkg")).unwrap();
            let fixture = Self {
                active: root.join("opkg/onekvm-feed.conf"),
                defaults: root.join("defaults.conf"),
                directory: root.join("profiles"),
                mirrors: root.join("mirrors"),
                root,
            };
            let config = "# Keep product and version\nsrc onekvm-metadata https://pkg-dev.onekvm.org/nanokvm/0.2.0/main\nsrc onekvm-main https://pkg-dev.onekvm.org/nanokvm/0.2.0/main\nsrc onekvm-extensions https://pkg-dev.onekvm.org/nanokvm/0.2.0/onekvm-extensions\n";
            fs::write(&fixture.defaults, config).unwrap();
            fs::write(&fixture.active, config).unwrap();
            fs::write(
                &fixture.mirrors,
                "https://pkg-dev.sgp.onekvm.org\nhttps://pkg-dev.tyo.onekvm.org\n",
            )
            .unwrap();
            fixture
        }

        fn refresh(
            &self,
            probe: impl Fn(&str) -> Option<Duration> + Sync,
            mut update: impl FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            refresh_at(
                &self.active,
                &self.defaults,
                &self.directory,
                &self.mirrors,
                |url| probe(url).ok_or(Failure::Unreachable),
                || update().map_err(|e| FetchError::new(Failure::Failed, e)),
            )
            .map(|_| ())
            .map_err(|error| error.detail)
        }

        fn download(
            &self,
            probe: impl Fn(&str) -> Option<Duration> + Sync,
            attempt: impl FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            download_at(
                &self.active,
                &self.defaults,
                &self.directory,
                &self.mirrors,
                |url| probe(url).ok_or(Failure::Unreachable),
                attempt,
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn mirror_latency(url: &str) -> Option<Duration> {
        if url.contains(".sgp.") {
            Some(Duration::from_millis(50))
        } else if url.contains(".tyo.") {
            Some(Duration::from_millis(10))
        } else {
            None
        }
    }

    #[test]
    fn healthy_default_becomes_a_symlink_without_probing_mirrors() {
        let fixture = Fixture::new();
        fixture
            .refresh(
                |url| {
                    assert!(url.starts_with("https://pkg-dev.onekvm.org/"));
                    Some(Duration::from_millis(1))
                },
                || {
                    assert_eq!(fs::read_link(&fixture.active).unwrap(), fixture.defaults);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(fs::read_link(&fixture.active).unwrap(), fixture.defaults);
    }

    #[test]
    fn fastest_reachable_mirror_is_shared_with_native_opkg_and_default_can_recover() {
        let fixture = Fixture::new();
        fixture.refresh(mirror_latency, || Ok(())).unwrap();
        let selected = fs::read_to_string(&fixture.active).unwrap();
        assert!(selected.contains("https://pkg-dev.tyo.onekvm.org/nanokvm/0.2.0/main"));
        assert_eq!(feed_lines(&selected).len(), 3);
        assert!(fs::read_link(&fixture.active)
            .unwrap()
            .starts_with(&fixture.directory));
        // Only the active profile is in opkg's *.conf directory.
        assert_eq!(
            fs::read_dir(fixture.active.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        fixture
            .refresh(|_| Some(Duration::ZERO), || Ok(()))
            .unwrap();
        assert_eq!(fs::read_link(&fixture.active).unwrap(), fixture.defaults);
    }

    #[test]
    fn signature_or_freshness_failure_tries_the_next_mirror() {
        let fixture = Fixture::new();
        let mut attempts = Vec::new();
        fixture
            .refresh(mirror_latency, || {
                let text = fs::read_to_string(&fixture.active).unwrap();
                attempts.push(text.clone());
                if text.contains(".tyo.") {
                    Err("signature verification failed".into())
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(fs::read_to_string(&fixture.active)
            .unwrap()
            .contains(".sgp."));
    }

    #[test]
    fn failed_updates_restore_regular_files_and_previous_symlinks() {
        let fixture = Fixture::new();
        let original = fs::read(&fixture.active).unwrap();
        assert!(fixture
            .refresh(mirror_latency, || Err("expired index".into()))
            .is_err());
        assert!(!fs::symlink_metadata(&fixture.active)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&fixture.active).unwrap(), original);
        fixture.refresh(mirror_latency, || Ok(())).unwrap();
        let target = fs::read_link(&fixture.active).unwrap();
        assert!(fixture
            .refresh(|_| Some(Duration::ZERO), || Err("download failed".into()))
            .is_err());
        assert_eq!(fs::read_link(&fixture.active).unwrap(), target);
        assert_eq!(
            fs::read_to_string(&fixture.active).unwrap(),
            fs::read_to_string(target).unwrap()
        );
    }

    #[test]
    fn all_unreachable_leaves_the_configuration_unchanged() {
        let fixture = Fixture::new();
        let original = fs::read(&fixture.active).unwrap();
        let probes = Mutex::new(Vec::new());
        assert!(fixture
            .refresh(
                |url| {
                    probes.lock().unwrap().push(url.to_string());
                    None
                },
                || panic!("unreachable source must not run opkg")
            )
            .is_err());
        assert_eq!(probes.into_inner().unwrap().len(), 3);
        assert_eq!(fs::read(&fixture.active).unwrap(), original);
    }

    #[test]
    fn disabled_and_customized_sources_are_not_replaced() {
        let fixture = Fixture::new();
        fs::rename(
            &fixture.active,
            fixture.active.with_extension("conf.disabled"),
        )
        .unwrap();
        fixture
            .refresh(|_| panic!("disabled source"), || Ok(()))
            .unwrap();
        assert!(!fixture.active.exists());
        fs::write(&fixture.active, "src local https://custom.invalid/feed\n").unwrap();
        fixture
            .refresh(|_| panic!("custom source"), || Ok(()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(&fixture.active).unwrap(),
            "src local https://custom.invalid/feed\n"
        );
    }

    const DOWNLOAD_FAILED: &str = "opkg failed with 255: Failed to download https://pkg-dev.onekvm.org/nanokvm/0.2.0/onekvm-extensions/onekvm-extension-x.ipk, wget returned 4.";

    #[test]
    fn failed_downloads_move_to_the_fastest_other_mirror_and_stay_there() {
        let fixture = Fixture::new();
        let mut servers = Vec::new();
        fixture
            .download(mirror_latency, || {
                let text = fs::read_to_string(&fixture.active).unwrap();
                servers.push(text.clone());
                if text.contains("pkg-dev.onekvm.org") {
                    Err(DOWNLOAD_FAILED.into())
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(servers.len(), 2);
        assert!(fs::read_to_string(&fixture.active)
            .unwrap()
            .contains("pkg-dev.tyo.onekvm.org"));
    }

    #[test]
    fn other_install_errors_and_unmanaged_feeds_do_not_switch_mirrors() {
        let fixture = Fixture::new();
        let original = fs::read(&fixture.active).unwrap();
        let error = fixture
            .download(|_| panic!("no probe"), || Err("unknown package 'x'".into()))
            .unwrap_err();
        assert_eq!(error, "unknown package 'x'");
        fs::write(&fixture.active, "src local https://custom.invalid/feed\n").unwrap();
        assert!(fixture
            .download(|_| panic!("custom source"), || Err(DOWNLOAD_FAILED.into()))
            .is_err());
        assert_eq!(
            fs::read_to_string(&fixture.active).unwrap(),
            "src local https://custom.invalid/feed\n"
        );
        assert_ne!(fs::read(&fixture.active).unwrap(), original);
    }

    #[test]
    fn downloads_failing_everywhere_restore_the_selected_server() {
        let fixture = Fixture::new();
        fixture.refresh(mirror_latency, || Ok(())).unwrap();
        let selected = fs::read_link(&fixture.active).unwrap();
        let mut attempts = 0;
        let error = fixture
            .download(
                |_| Some(Duration::ZERO),
                || {
                    attempts += 1;
                    Err(DOWNLOAD_FAILED.into())
                },
            )
            .unwrap_err();
        assert_eq!(attempts, 3);
        assert!(error.contains("pkg-dev.sgp.onekvm.org"));
        assert_eq!(fs::read_link(&fixture.active).unwrap(), selected);
    }

    #[test]
    fn unpublished_versions_are_told_apart_from_unreachable_servers() {
        let fixture = Fixture::new();
        let error = refresh_at(
            &fixture.active,
            &fixture.defaults,
            &fixture.directory,
            &fixture.mirrors,
            |url| {
                if url.contains(".sgp.") {
                    Err(Failure::Unreachable)
                } else {
                    Err(Failure::NotFound)
                }
            },
            || panic!("nothing to update"),
        )
        .unwrap_err();
        assert_eq!(error.failure, Failure::NotFound);
        assert!(error.detail.contains("pkg-dev.sgp.onekvm.org: timed out"));

        assert_eq!(
            wget_failure(Some(8), "  HTTP/1.1 302 Found\n  HTTP/1.1 404 Not Found\n"),
            Failure::NotFound
        );
        assert_eq!(
            wget_failure(Some(8), "  HTTP/1.1 503 Service Unavailable\n"),
            Failure::ServerError
        );
        assert_eq!(wget_failure(Some(4), ""), Failure::Unreachable);
        assert_eq!(wget_failure(None, ""), Failure::Unreachable);
        assert_eq!(wget_failure(Some(5), ""), Failure::Certificate);

        let failed = "opkg update failed with 1: error: opkg_download_backend: Failed to download https://a.example/feed/Packages, wget returned 8.";
        let checked = update_failure_with(failed, |url| {
            assert_eq!(url, "https://a.example/feed/Packages");
            Err(Failure::NotFound)
        });
        assert_eq!(checked.failure, Failure::NotFound);
        assert_eq!(
            update_failure_with(failed, |_| Ok(Duration::ZERO)).failure,
            Failure::ServerError
        );
        assert_eq!(
            update_failure_with("opkg update timed out after 60 seconds: ", |_| panic!()).failure,
            Failure::Unreachable
        );
        assert_eq!(
            update_failure_with("error: Signature verification failed", |_| panic!()).failure,
            Failure::Rejected
        );
    }

    #[test]
    fn stable_without_configured_mirrors_stays_on_stable() {
        let fixture = Fixture::new();
        let config = fs::read_to_string(&fixture.defaults)
            .unwrap()
            .replace("pkg-dev.", "pkg.");
        fs::write(&fixture.defaults, &config).unwrap();
        fs::write(&fixture.active, &config).unwrap();
        fs::write(&fixture.mirrors, "").unwrap();
        fixture
            .refresh(|_| panic!("no fallback needs probing"), || Ok(()))
            .unwrap();
        assert_eq!(fs::read_link(&fixture.active).unwrap(), fixture.defaults);
        assert!(!fs::read_to_string(&fixture.active)
            .unwrap()
            .contains("pkg-dev"));
    }
}
