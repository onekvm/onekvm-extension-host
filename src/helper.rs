use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_OUTPUT: usize = 1 << 20;
const SUPERVISOR_STACK_SIZE: usize = 128 * 1024;

#[derive(Clone)]
struct CommandSpec {
    command: Vec<String>,
    environment: Vec<String>,
}

struct ManagedProcess {
    spec: CommandSpec,
    restart: String,
    stop: OwnedFd,
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

struct HelperState {
    id: String,
    reuse_key: String,
    address_file: PathBuf,
    controller: CommandSpec,
    controller_timeout: Duration,
    cgroups: CgroupManager,
    instances: Mutex<HashMap<String, ManagedProcess>>,
}

#[derive(Clone)]
struct CgroupManager {
    instances: PathBuf,
}

struct ManagedChild {
    child: Child,
    pidfd: OwnedFd,
    cgroup: PathBuf,
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Child::drop does not terminate the process. Keep the cgroup clean if
        // supervisor thread creation or another ownership transfer fails.
        kill_cgroup(&self.cgroup);
        kill_process_group(&mut self.child);
        remove_cgroup(&self.cgroup);
    }
}

struct Captured {
    data: Vec<u8>,
    oversized: bool,
}

fn method_error(context: &str, error: impl std::fmt::Display) -> String {
    format!("{context}: {error}")
}

fn command_from_spec(spec: &CommandSpec) -> Result<Command, String> {
    let (program, arguments) = spec
        .command
        .split_first()
        .ok_or_else(|| "command is empty".to_owned())?;
    if program.is_empty() || spec.command.iter().any(|value| value.contains('\0')) {
        return Err("command contains an empty or invalid argument".to_owned());
    }
    let mut command = Command::new(program);
    command.args(arguments);
    for value in &spec.environment {
        let (name, value) = value
            .split_once('=')
            .ok_or_else(|| "environment entry is missing '='".to_owned())?;
        if name.is_empty() || name.contains('\0') || value.contains('\0') {
            return Err("environment entry is invalid".to_owned());
        }
        command.env(name, value);
    }
    command.process_group(0);
    Ok(command)
}

impl CgroupManager {
    fn discover() -> io::Result<Self> {
        let cgroup = fs::read_to_string("/proc/self/cgroup")?;
        let relative = cgroup
            .lines()
            .find_map(|line| {
                let mut fields = line.splitn(3, ':');
                let _hierarchy = fields.next()?;
                let controllers = fields.next()?;
                let path = fields.next()?;
                controllers.is_empty().then_some(path)
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cgroup v2 path is missing"))?;
        let root = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        Self::setup(root)
    }

    fn setup(root: PathBuf) -> io::Result<Self> {
        // Keep the helper and short-lived controller/action children in a
        // leaf cgroup. This leaves the delegated unit root free to enable the
        // memory controller for individually-accounted managed processes.
        let supervisor = root.join("supervisor");
        fs::create_dir_all(&supervisor)?;
        fs::write(
            supervisor.join("cgroup.procs"),
            format!("{}\n", std::process::id()),
        )?;

        enable_subtree_controllers(&root)?;

        let instances = root.join("instances");
        fs::create_dir_all(&instances)?;
        // Managed processes are grandchildren of the systemd service cgroup:
        // service/instances/<name>. Controllers must therefore be propagated
        // through both internal levels before <name>/memory.current exists.
        enable_subtree_controllers(&instances)?;
        Ok(Self { instances })
    }

    fn attach(&self, name: &str, pid: u32) -> io::Result<PathBuf> {
        let path = self.instances.join(name);
        fs::create_dir_all(&path)?;
        fs::write(path.join("cgroup.procs"), format!("{pid}\n"))?;
        Ok(path)
    }
}

fn enable_subtree_controllers(path: &Path) -> io::Result<()> {
    let available = fs::read_to_string(path.join("cgroup.controllers"))?;
    let enabled = fs::read_to_string(path.join("cgroup.subtree_control"))?;
    let enabled: Vec<&str> = enabled.split_whitespace().collect();
    let additions: Vec<String> = available
        .split_whitespace()
        .filter(|controller| !enabled.contains(controller))
        .map(|controller| format!("+{controller}"))
        .collect();
    if !additions.is_empty() {
        fs::write(path.join("cgroup.subtree_control"), additions.join(" "))?;
    }
    Ok(())
}

fn open_pidfd(pid: u32) -> io::Result<OwnedFd> {
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn create_stop_event() -> io::Result<(OwnedFd, OwnedFd)> {
    let descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let receiver = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let sender = receiver.try_clone()?;
    Ok((sender, receiver))
}

fn signal_stop(stop: &OwnedFd) -> io::Result<()> {
    let value = 1_u64.to_ne_bytes();
    let written = unsafe {
        libc::write(
            stop.as_raw_fd(),
            value.as_ptr().cast::<libc::c_void>(),
            value.len(),
        )
    };
    if written == value.len() as isize {
        Ok(())
    } else if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short eventfd write",
        ))
    }
}

enum PollResult {
    ProcessExited,
    StopRequested,
    TimedOut,
}

fn poll_process(
    pidfd: &OwnedFd,
    stop: Option<&OwnedFd>,
    timeout: Option<Duration>,
) -> io::Result<PollResult> {
    let deadline = timeout.map(|value| Instant::now() + value);
    loop {
        let timeout_millis = match deadline {
            None => -1,
            Some(value) => {
                let remaining = value.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(PollResult::TimedOut);
                }
                remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128) as i32
            }
        };
        let mut descriptors = [
            libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.map(|value| value.as_raw_fd()).unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let count = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout_millis,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Ok(PollResult::TimedOut);
        }
        if descriptors[1].revents != 0 {
            return Ok(PollResult::StopRequested);
        }
        if descriptors[0].revents != 0 {
            return Ok(PollResult::ProcessExited);
        }
    }
}

fn poll_stop(stop: &OwnedFd, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let mut descriptor = libc::pollfd {
            fd: stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let count = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128) as i32,
            )
        };
        if count > 0 {
            return Ok(true);
        }
        if count == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn kill_process_group(child: &mut Child) {
    // Every helper child starts a new process group. Kill the complete group so
    // a controller cannot leave grandchildren behind after a timeout/removal.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_cgroup(path: &Path) {
    let kill = path.join("cgroup.kill");
    if kill.exists() {
        let _ = fs::write(kill, "1\n");
        return;
    }
    // cgroup.kill was added after the 5.10 kernel used by NanoKVM. Managed
    // children normally stay in their own process group, but killing every
    // PID also covers a child which deliberately created another session.
    if let Ok(processes) = fs::read_to_string(path.join("cgroup.procs")) {
        for process in processes.split_whitespace() {
            if let Ok(pid) = process.parse::<i32>() {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

fn remove_cgroup(path: &Path) {
    if let Err(error) = fs::remove_dir(path) {
        if error.kind() != io::ErrorKind::NotFound && error.raw_os_error() != Some(libc::EBUSY) {
            eprintln!(
                "onekvm-extension-helper: remove instance cgroup {}: {error}",
                path.display()
            );
        }
    }
}

fn terminate_managed(child: &mut ManagedChild) {
    kill_cgroup(&child.cgroup);
    kill_process_group(&mut child.child);
    remove_cgroup(&child.cgroup);
}

fn finish_managed(child: &mut ManagedChild) -> io::Result<ExitStatus> {
    let status = child.child.wait()?;
    // A managed executable must not leave grandchildren behind after its main
    // process exits. Kill the delegated instance cgroup before reusing it.
    kill_cgroup(&child.cgroup);
    unsafe {
        libc::kill(-(child.child.id() as i32), libc::SIGKILL);
    }
    remove_cgroup(&child.cgroup);
    Ok(status)
}

fn wait_until(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let pidfd = open_pidfd(child.id())?;
    match poll_process(&pidfd, None, Some(timeout))? {
        PollResult::ProcessExited => child.wait(),
        PollResult::TimedOut => {
            kill_process_group(child);
            Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"))
        }
        PollResult::StopRequested => unreachable!(),
    }
}

fn read_limited(mut input: impl Read) -> io::Result<Captured> {
    let mut data = Vec::new();
    let mut oversized = false;
    let mut buffer = [0u8; 8192];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if data.len() < MAX_OUTPUT {
            let retained = count.min(MAX_OUTPUT - data.len());
            data.extend_from_slice(&buffer[..retained]);
            oversized |= retained != count;
        } else {
            oversized = true;
        }
    }
    Ok(Captured { data, oversized })
}

fn run_capture(spec: &CommandSpec, input: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    if input.len() > MAX_OUTPUT {
        return Err("controller input exceeds 1 MiB".to_owned());
    }
    let mut command = command_from_spec(spec)?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start controller: {error}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "controller stdin is unavailable".to_owned())?;
        stdin
            .write_all(input)
            .map_err(|error| format!("write controller input: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("flush controller input: {error}"))?;
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "controller stdout is unavailable".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "controller stderr is unavailable".to_owned())?;
    let stdout_thread = thread::spawn(move || read_limited(stdout));
    let stderr_thread = thread::spawn(move || read_limited(stderr));
    let status = wait_until(&mut child, timeout).map_err(|error| error.to_string());
    let stdout = stdout_thread
        .join()
        .map_err(|_| "controller stdout reader panicked".to_owned())?
        .map_err(|error| format!("read controller stdout: {error}"))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| "controller stderr reader panicked".to_owned())?
        .map_err(|error| format!("read controller stderr: {error}"))?;
    let status = status?;
    if stdout.oversized || stderr.oversized {
        return Err("controller output exceeds 1 MiB".to_owned());
    }
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr.data).trim().to_owned();
        return Err(if message.is_empty() {
            format!("controller exited with {status}")
        } else {
            message
        });
    }
    Ok(stdout.data)
}

fn run_exec(spec: &CommandSpec, timeout: Duration) -> Result<(), String> {
    let mut command = command_from_spec(spec)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start action: {error}"))?;
    let status = wait_until(&mut child, timeout).map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("action exited with {status}"));
    }
    Ok(())
}

fn spawn_managed(
    spec: &CommandSpec,
    cgroups: &CgroupManager,
    name: &str,
) -> Result<ManagedChild, String> {
    let mut command = command_from_spec(spec)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start managed process: {error}"))?;
    let pidfd = match open_pidfd(child.id()) {
        Ok(value) => value,
        Err(error) => {
            kill_process_group(&mut child);
            return Err(format!("open managed process pidfd: {error}"));
        }
    };
    let cgroup = match cgroups.attach(name, child.id()) {
        Ok(value) => value,
        Err(error) => {
            kill_process_group(&mut child);
            return Err(format!("attach managed process cgroup: {error}"));
        }
    };
    Ok(ManagedChild {
        child,
        pidfd,
        cgroup,
    })
}

fn supervise(
    mut child: ManagedChild,
    spec: CommandSpec,
    restart: String,
    stop: OwnedFd,
    running: Arc<AtomicBool>,
    cgroups: CgroupManager,
    name: String,
) {
    running.store(true, Ordering::Release);
    loop {
        match poll_process(&child.pidfd, Some(&stop), None) {
            Ok(PollResult::StopRequested) => {
                terminate_managed(&mut child);
                running.store(false, Ordering::Release);
                return;
            }
            Ok(PollResult::ProcessExited) => match finish_managed(&mut child) {
                Ok(status) => {
                    running.store(false, Ordering::Release);
                    let should_restart =
                        restart == "always" || (restart == "on-failure" && !status.success());
                    if !should_restart {
                        return;
                    }
                }
                Err(error) => {
                    running.store(false, Ordering::Release);
                    eprintln!("onekvm-extension-helper: wait for child: {error}");
                    if restart == "no" {
                        return;
                    }
                }
            },
            Ok(PollResult::TimedOut) => unreachable!(),
            Err(error) => {
                running.store(false, Ordering::Release);
                eprintln!("onekvm-extension-helper: poll child: {error}");
                terminate_managed(&mut child);
                if restart == "no" {
                    return;
                }
            }
        }
        match poll_stop(&stop, Duration::from_secs(1)) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                eprintln!("onekvm-extension-helper: wait before restart: {error}");
                return;
            }
        }
        loop {
            match spawn_managed(&spec, &cgroups, &name) {
                Ok(next) => {
                    child = next;
                    running.store(true, Ordering::Release);
                    break;
                }
                Err(error) => {
                    eprintln!("onekvm-extension-helper: restart child: {error}");
                    match poll_stop(&stop, Duration::from_secs(1)) {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!(
                                "onekvm-extension-helper: wait after restart failure: {error}"
                            );
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn stop_managed(process: ManagedProcess) {
    let _ = signal_stop(&process.stop);
    let _ = process.thread.join();
}

impl HelperState {
    fn ensure(&self, name: String, spec: CommandSpec, restart: String) -> Result<(), String> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || value == b'_' || value == b'-')
        {
            return Err("managed process name is invalid".to_owned());
        }
        if restart != "no" && restart != "on-failure" && restart != "always" {
            return Err("managed process restart policy is invalid".to_owned());
        }
        let previous = {
            let mut instances = self
                .instances
                .lock()
                .map_err(|_| "instance lock is poisoned")?;
            if instances
                .get(&name)
                .is_some_and(|process| process.running.load(Ordering::Acquire))
            {
                return Ok(());
            }
            instances.remove(&name)
        };
        if let Some(previous) = previous {
            stop_managed(previous);
        }
        let child = spawn_managed(&spec, &self.cgroups, &name)?;
        let (stop_sender, stop_receiver) = create_stop_event()
            .map_err(|error| format!("create supervisor stop event: {error}"))?;
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread_spec = spec.clone();
        let thread_restart = restart.clone();
        let thread_cgroups = self.cgroups.clone();
        let thread_name = name.clone();
        let process_thread = thread::Builder::new()
            .name(format!("instance-{name}"))
            .stack_size(SUPERVISOR_STACK_SIZE)
            .spawn(move || {
                supervise(
                    child,
                    thread_spec,
                    thread_restart,
                    stop_receiver,
                    thread_running,
                    thread_cgroups,
                    thread_name,
                )
            })
            .map_err(|error| format!("start supervisor thread: {error}"))?;
        self.instances
            .lock()
            .map_err(|_| "instance lock is poisoned")?
            .insert(
                name,
                ManagedProcess {
                    spec,
                    restart,
                    stop: stop_sender,
                    running,
                    thread: process_thread,
                },
            );
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        let process = self
            .instances
            .lock()
            .map_err(|_| "instance lock is poisoned")?
            .remove(name);
        if let Some(process) = process {
            stop_managed(process);
        }
        Ok(())
    }

    fn remove_all(&self) -> Result<(), String> {
        let processes: Vec<_> = self
            .instances
            .lock()
            .map_err(|_| "instance lock is poisoned")?
            .drain()
            .map(|(_, value)| value)
            .collect();
        for process in processes {
            stop_managed(process);
        }
        Ok(())
    }

    fn restart(&self, name: &str) -> Result<(), String> {
        let process = self
            .instances
            .lock()
            .map_err(|_| "instance lock is poisoned")?
            .remove(name)
            .ok_or_else(|| format!("managed process {name} is not registered"))?;
        let spec = process.spec.clone();
        let restart = process.restart.clone();
        stop_managed(process);
        self.ensure(name.to_owned(), spec, restart)
    }

    fn instances(&self) -> Result<Vec<String>, String> {
        let mut names: Vec<_> = self
            .instances
            .lock()
            .map_err(|_| "instance lock is poisoned")?
            .iter()
            .filter(|(_, process)| process.running.load(Ordering::Acquire))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        Ok(names)
    }
}

fn write_address(path: &Path, address: &str, reuse_key: &str) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    writeln!(output, "{address}")?;
    writeln!(output, "{reuse_key}")?;
    output.sync_all()?;
    fs::rename(&temporary, path)
}

pub fn run(arguments: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.len() < 5 {
        return Err(
            "helper requires <id> <timeout-seconds> <address-file> <reuse-key> <controller> [arguments...]"
                .into(),
        );
    }
    let id = arguments[0].clone();
    let timeout_seconds: u64 = arguments[1].parse()?;
    if timeout_seconds == 0 || timeout_seconds > 300 {
        return Err("helper timeout must be between 1 and 300 seconds".into());
    }
    let address_file = PathBuf::from(&arguments[2]);
    let cgroups = CgroupManager::discover()?;
    let state = Arc::new(HelperState {
        id,
        reuse_key: arguments[3].clone(),
        address_file,
        controller: CommandSpec {
            command: arguments[4..].to_vec(),
            environment: Vec::new(),
        },
        controller_timeout: Duration::from_secs(timeout_seconds),
        cgroups,
        instances: Mutex::new(HashMap::new()),
    });
    let raw = Arc::into_raw(state);
    let status = unsafe { crate::ffi::onekvm_helper_serve(raw as *mut std::os::raw::c_void) };
    let state = unsafe { Arc::from_raw(raw) };
    let _ = state.remove_all();
    let _ = fs::remove_file(&state.address_file);
    if status < 0 {
        return Err(format!("sd-bus helper failed ({status})").into());
    }
    Ok(())
}

fn c_string(value: &str) -> *mut std::os::raw::c_char {
    std::ffi::CString::new(value)
        .unwrap_or_else(|_| std::ffi::CString::new("invalid").unwrap())
        .into_raw()
}

fn set_err(slot: *mut *mut std::os::raw::c_char, message: String) {
    unsafe { *slot = c_string(&message) };
}

unsafe fn string_vec(ptr: *mut *mut std::os::raw::c_char, count: usize) -> Vec<String> {
    if ptr.is_null() || count == 0 {
        return Vec::new();
    }
    std::slice::from_raw_parts(ptr, count)
        .iter()
        .map(|item| {
            if item.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(*item)
                    .to_string_lossy()
                    .into_owned()
            }
        })
        .collect()
}

unsafe fn helper_ref<'a>(ptr: *mut std::os::raw::c_void) -> Option<&'a HelperState> {
    ptr.cast::<HelperState>().as_ref()
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_set_address(
    helper: *mut std::os::raw::c_void,
    name: *const std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        return -1;
    };
    let name = std::ffi::CStr::from_ptr(name).to_string_lossy();
    match write_address(&state.address_file, &name, &state.reuse_key) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_ping(
    helper: *mut std::os::raw::c_void,
    id: *mut *mut std::os::raw::c_char,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    *id = c_string(&state.id);
    0
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_run_controller(
    helper: *mut std::os::raw::c_void,
    payload: *const std::os::raw::c_void,
    size: usize,
    output: *mut *mut std::os::raw::c_void,
    out_size: *mut usize,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    let bytes = std::slice::from_raw_parts(payload as *const u8, size);
    match run_capture(&state.controller, bytes, state.controller_timeout) {
        Ok(data) => {
            let size = data.len();
            let ptr = libc::malloc(size.max(1)) as *mut u8;
            if ptr.is_null() {
                set_err(err, "allocate controller output".into());
                return -1;
            }
            if size != 0 {
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, size);
            }
            *out_size = size;
            *output = ptr as *mut std::os::raw::c_void;
            0
        }
        Err(error) => {
            set_err(err, method_error("run controller", error));
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_ensure(
    helper: *mut std::os::raw::c_void,
    name: *const std::os::raw::c_char,
    command: *mut *mut std::os::raw::c_char,
    ncommand: usize,
    environment: *mut *mut std::os::raw::c_char,
    nenv: usize,
    restart: *const std::os::raw::c_char,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    let name = std::ffi::CStr::from_ptr(name)
        .to_string_lossy()
        .into_owned();
    let restart = std::ffi::CStr::from_ptr(restart)
        .to_string_lossy()
        .into_owned();
    match state.ensure(
        name,
        CommandSpec {
            command: string_vec(command, ncommand),
            environment: string_vec(environment, nenv),
        },
        restart,
    ) {
        Ok(()) => 0,
        Err(error) => {
            set_err(err, method_error("ensure process", error));
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_remove(
    helper: *mut std::os::raw::c_void,
    name: *const std::os::raw::c_char,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    let name = std::ffi::CStr::from_ptr(name).to_string_lossy();
    match state.remove(&name) {
        Ok(()) => 0,
        Err(error) => {
            set_err(err, method_error("remove process", error));
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_remove_all(
    helper: *mut std::os::raw::c_void,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    match state.remove_all() {
        Ok(()) => 0,
        Err(error) => {
            set_err(err, method_error("remove all processes", error));
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_restart(
    helper: *mut std::os::raw::c_void,
    name: *const std::os::raw::c_char,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    let name = std::ffi::CStr::from_ptr(name).to_string_lossy();
    match state.restart(&name) {
        Ok(()) => 0,
        Err(error) => {
            set_err(err, method_error("restart process", error));
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_exec(
    helper: *mut std::os::raw::c_void,
    command: *mut *mut std::os::raw::c_char,
    ncommand: usize,
    environment: *mut *mut std::os::raw::c_char,
    nenv: usize,
    timeout: u32,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(_state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    let timeout = Duration::from_secs(u64::from(timeout));
    if timeout.is_zero() || timeout > Duration::from_secs(300) {
        set_err(
            err,
            "action timeout must be between 1 and 300 seconds".into(),
        );
        return -1;
    }
    match run_exec(
        &CommandSpec {
            command: string_vec(command, ncommand),
            environment: string_vec(environment, nenv),
        },
        timeout,
    ) {
        Ok(()) => 0,
        Err(error) => {
            set_err(err, method_error("execute action", error));
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_instances(
    helper: *mut std::os::raw::c_void,
    names: *mut *mut *mut std::os::raw::c_char,
    count: *mut usize,
    err: *mut *mut std::os::raw::c_char,
) -> std::os::raw::c_int {
    let Some(state) = helper_ref(helper) else {
        set_err(err, "helper is missing".into());
        return -1;
    };
    match state.instances() {
        Ok(list) => {
            let n = list.len();
            let ptr = libc::malloc(
                std::mem::size_of::<*mut std::os::raw::c_char>().saturating_mul(n.max(1)),
            ) as *mut *mut std::os::raw::c_char;
            if ptr.is_null() {
                set_err(err, "allocate instance list".into());
                return -1;
            }
            for (index, name) in list.iter().enumerate() {
                let item = std::ffi::CString::new(name.as_str()).unwrap_or_default();
                *ptr.add(index) = libc::strdup(item.as_ptr());
            }
            *count = n;
            *names = ptr;
            0
        }
        Err(error) => {
            set_err(err, method_error("list processes", error));
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_helper_should_exit(
    helper: *mut std::os::raw::c_void,
) -> std::os::raw::c_int {
    let Some(_state) = helper_ref(helper) else {
        return 1;
    };
    // The systemd unit is the lifetime owner. Keeping an enabled extension's
    // helper alive preserves its mount/device/network namespaces for later
    // controller invocations, even when it currently supervises no process.
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_rejects_invalid_environment() {
        let spec = CommandSpec {
            command: vec!["/bin/true".into()],
            environment: vec!["INVALID".into()],
        };
        assert!(command_from_spec(&spec).is_err());
    }

    #[test]
    fn capture_enforces_success_and_returns_stdout() {
        let spec = CommandSpec {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "read value; printf '%s' \"$value\"".into(),
            ],
            environment: Vec::new(),
        };
        assert_eq!(
            run_capture(&spec, b"hello\n", Duration::from_secs(1)).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn helper_supervises_process_inside_its_own_tree() {
        let instance_root = std::env::temp_dir().join(format!(
            "onekvm-extension-helper-test-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&instance_root).unwrap();
        let state = HelperState {
            id: "test".into(),
            reuse_key: "test-key".into(),
            address_file: instance_root.join("helper.bus"),
            controller: CommandSpec {
                command: vec!["/bin/true".into()],
                environment: Vec::new(),
            },
            controller_timeout: Duration::from_secs(1),
            cgroups: CgroupManager {
                instances: instance_root.clone(),
            },
            instances: Mutex::new(HashMap::new()),
        };
        state
            .ensure(
                "worker".into(),
                CommandSpec {
                    command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                    environment: Vec::new(),
                },
                "no".into(),
            )
            .unwrap();
        assert_eq!(state.instances().unwrap(), vec!["worker"]);
        state.remove_all().unwrap();
        assert!(state.instances().unwrap().is_empty());
        let _ = fs::remove_dir_all(instance_root);
    }
}
