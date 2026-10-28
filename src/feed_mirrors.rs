//! The host selects the distribution feed; native opkg follows the same symlink.
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::{paths, sys};

pub fn refresh(update: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    refresh_at(
        Path::new(paths::FEED_CONFIG),
        Path::new(paths::FEED_DEFAULT),
        Path::new(paths::FEED_PROFILES),
        Path::new(paths::FEED_MIRRORS),
        probe,
        update,
    )
}

fn probe(url: &str) -> Option<Duration> {
    let started = Instant::now();
    sys::run_with_timeout(
        "/usr/bin/wget",
        &[
            "--quiet".into(),
            "--tries=1".into(),
            "--timeout=3".into(),
            "--output-document=/dev/null".into(),
            "--".into(),
            url.into(),
        ],
        Duration::from_secs(4),
    )
    .ok()
    .map(|_| started.elapsed())
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

fn refresh_at(
    active: &Path,
    defaults: &Path,
    directory: &Path,
    mirrors: &Path,
    probe: impl Fn(&str) -> Option<Duration> + Sync,
    mut update: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    // Disabled, locally customized and legacy configurations remain administrator owned.
    let (Ok(current), Ok(default), Ok(info)) = (
        fs::read_to_string(active),
        fs::read_to_string(defaults),
        fs::symlink_metadata(active),
    ) else {
        return update();
    };
    let old_target = fs::read_link(active).ok();
    let managed = old_target
        .as_ref()
        .is_some_and(|target| target == defaults || *target == profile_path(directory, &current));
    if !managed && feed_lines(&current) != feed_lines(&default) {
        return update();
    }
    let mirror_text = match fs::read_to_string(mirrors) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("read feed mirrors: {err}")),
    };
    let Some(candidates) = profiles(&default, defaults, directory, &mirror_text) else {
        return update();
    };
    let mut switched = false;
    let result = (|| {
        let mut failures = Vec::new();
        let mut attempt = |candidate: &Profile| -> Result<(), String> {
            if candidate.target != defaults {
                sys::atomic_write(&candidate.target, candidate.text.as_bytes(), 0o644)?;
            }
            switched = true;
            atomic_link(&candidate.target, active)?;
            eprintln!(
                "Updating distribution feed from {}",
                origin(&candidate.probe_url).unwrap_or_default()
            );
            update()
        };
        if candidates.len() == 1 || probe(&candidates[0].probe_url).is_some() {
            match attempt(&candidates[0]) {
                Ok(()) => return Ok(()),
                Err(err) => failures.push(err),
            }
        } else {
            failures.push("default feed is unreachable".into());
        }
        // Probe both mirrors concurrently; HTTPS GET time includes DNS, TLS and response latency.
        let mut reachable = std::thread::scope(|scope| {
            let jobs: Vec<_> = candidates[1..]
                .iter()
                .map(|candidate| {
                    let probe = &probe;
                    scope.spawn(move || {
                        probe(&candidate.probe_url).map(|elapsed| (elapsed, candidate))
                    })
                })
                .collect();
            jobs.into_iter()
                .filter_map(|job| job.join().ok().flatten())
                .collect::<Vec<_>>()
        });
        reachable.sort_by_key(|(elapsed, _)| *elapsed);
        for (elapsed, candidate) in reachable {
            eprintln!(
                "Feed mirror {} responded in {} ms",
                origin(&candidate.probe_url).unwrap_or_default(),
                elapsed.as_millis()
            );
            match attempt(candidate) {
                Ok(()) => return Ok(()),
                Err(err) => failures.push(err),
            }
        }
        Err(format!(
            "distribution feed refresh failed: {}",
            failures.join("; ")
        ))
    })();
    if result.is_err() && switched {
        let restored = match old_target {
            Some(target) => atomic_link(&target, active),
            None => sys::atomic_write(
                active,
                current.as_bytes(),
                info.permissions().mode() & 0o777,
            ),
        };
        restored.map_err(|err| {
            format!(
                "{}; restore feed configuration: {err}",
                result.as_ref().unwrap_err()
            )
        })?;
    }
    result
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
            update: impl FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            refresh_at(
                &self.active,
                &self.defaults,
                &self.directory,
                &self.mirrors,
                probe,
                update,
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
