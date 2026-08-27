use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::manifest::{load_installed, Installed, Manifest};
use crate::paths;
use crate::sandbox::{
    helper_sandbox_arguments, is_active, load_kernel_modules, sandbox_arguments, systemd_run,
};
use crate::settings::{
    repair_settings_output_permission, repair_settings_output_permissions, settings_for_state,
    write_runtime_settings,
};
use crate::state::{
    initialize_default_enabled_states, prepare_credential, prepare_runtime_dirs, read_generation,
    read_state, remove_credential, write_generation, PluginState,
};
use crate::sys;

pub struct Host {
    pub generation: std::sync::Mutex<String>,
}

struct ReconcileCandidate {
    id: String,
    state: PluginState,
}

struct LegacyStart {
    id: String,
    extension: Installed,
    state: PluginState,
}

impl Host {
    pub fn new() -> Self {
        Self {
            generation: std::sync::Mutex::new(read_generation()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ControllerPlan {
    #[serde(default)]
    actions: Vec<ControllerAction>,
    #[serde(default)]
    response: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ControllerAction {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    environment: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    restart: String,
    #[serde(default)]
    timeout_seconds: u32,
}

pub fn apply(
    host: &Host,
    id: &str,
    revision: u64,
    event: &str,
    reason: &str,
    changed_keys: &[String],
    method: &str,
    request: Option<Value>,
) -> Result<Value, String> {
    if !paths::valid_id(id) {
        return Err(format!("invalid extension id {id:?}"));
    }
    let state = read_state(id)?;
    if revision != 0 && revision != state.revision {
        return Err(format!(
            "stale extension revision {revision}; current revision is {}",
            state.revision
        ));
    }
    let extension = load_installed(id, &state.active_version)?;
    if extension.manifest.controller.is_none() {
        return apply_legacy(&extension, &state, event);
    }
    if !method.is_empty() {
        let spec = extension
            .manifest
            .methods
            .get(method)
            .ok_or_else(|| format!("extension {id} does not export method {method:?}"))?;
        if !state.enabled && !spec.available_when_disabled {
            return Err(format!(
                "method {method:?} is unavailable while extension {id} is disabled"
            ));
        }
    }
    apply_controller(
        host,
        &extension,
        &state,
        event,
        reason,
        changed_keys,
        method,
        request,
    )
}

fn apply_legacy(extension: &Installed, state: &PluginState, event: &str) -> Result<Value, String> {
    match event {
        "disable" => stop_legacy(&extension.manifest.id),
        "enable" | "reconcile" | "config_changed" if state.enabled => {
            start_legacy(extension, state)
        }
        "enable" | "reconcile" | "config_changed" => stop_legacy(&extension.manifest.id),
        _ => Err(format!(
            "extension {} has no controller",
            extension.manifest.id
        )),
    }
    .map(|_| Value::Null)
}

fn apply_controller(
    host: &Host,
    extension: &Installed,
    state: &PluginState,
    event: &str,
    reason: &str,
    changed_keys: &[String],
    method: &str,
    request: Option<Value>,
) -> Result<Value, String> {
    let id = &extension.manifest.id;
    prepare_runtime_dirs(id)?;
    let settings = settings_for_state(extension, state)?;
    write_runtime_settings(id, &settings)?;
    let helper_started = ensure_helper(extension)?;
    let generation = host
        .generation
        .lock()
        .map_err(|_| "host state lock is poisoned".to_string())?
        .clone();
    if helper_started && state.enabled && event == "invoke" {
        let payload = json!({
            "api_version": 1,
            "event": "reconcile",
            "extension_id": id,
            "revision": state.revision,
            "enabled": true,
            "settings": settings,
            "reason": "helper_restarted",
            "core_generation": generation,
        });
        let plan = run_controller(extension, &payload, "reconcile")?;
        execute_plan(extension, &plan)?;
    }
    let mut payload = json!({
        "api_version": 1,
        "event": event,
        "extension_id": id,
        "revision": state.revision,
        "enabled": state.enabled,
        "settings": settings,
        "reason": reason,
        "core_generation": generation,
    });
    if !changed_keys.is_empty() {
        payload["changed_keys"] = json!(changed_keys);
    }
    if !method.is_empty() {
        payload["method"] = json!(method);
    }
    if let Some(request) = request {
        payload["request"] = request;
    }
    let plan = run_controller(extension, &payload, event)?;
    execute_plan(extension, &plan)?;
    repair_settings_output_permission(id)?;
    if !state.enabled {
        stop_helper(id)?;
    }
    Ok(plan.response.unwrap_or(Value::Null))
}

pub fn reconcile_all(host: &Host, generation: &str) -> Result<(), String> {
    write_generation(generation)?;
    *host
        .generation
        .lock()
        .map_err(|_| "host state lock is poisoned".to_string())? = generation.to_owned();
    sys::with_lock(initialize_default_enabled_states)?;
    let (mut candidates, mut failures) = read_candidates()?;
    prioritize_enabled(&mut candidates);
    let (legacy, serial) = split_legacy_starts(candidates, &mut failures);
    start_legacy_parallel(&legacy, &mut failures);
    for candidate in serial {
        if let Err(err) = reconcile_one(host, &candidate.id, &candidate.state) {
            failures.push(format!("{}: {err}", candidate.id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("reconcile failures: {}", failures.join("; ")))
    }
}

// Start enabled legacy services as soon as Core has created the extension
// control and media sockets. ReconcileAll remains the authoritative readiness
// barrier and will retry any failed or incomplete prewarm operation.
pub fn prewarm() -> Result<(), String> {
    eprintln!("extension prewarm: discovering enabled legacy services");
    sys::with_lock(initialize_default_enabled_states)?;
    let (mut candidates, mut failures) = read_candidates()?;
    prioritize_enabled(&mut candidates);
    let (legacy, _) = split_legacy_starts(candidates, &mut failures);
    start_legacy_parallel(&legacy, &mut failures);
    if failures.is_empty() {
        eprintln!("extension prewarm: enabled legacy services are active");
        Ok(())
    } else {
        Err(format!("prewarm failures: {}", failures.join("; ")))
    }
}

fn read_candidates() -> Result<(Vec<ReconcileCandidate>, Vec<String>), String> {
    let entries = match fs::read_dir(paths::CONFIG_ROOT) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()))
        }
        Err(err) => return Err(err.to_string()),
    };
    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) || !name.ends_with(".json") {
            continue;
        }
        let id = name.trim_end_matches(".json");
        if !paths::valid_id(id) {
            continue;
        }
        match read_state(id) {
            Ok(state) => candidates.push(ReconcileCandidate {
                id: id.to_owned(),
                state,
            }),
            Err(err) => failures.push(format!("{id}: {err}")),
        }
    }
    Ok((candidates, failures))
}

fn split_legacy_starts(
    candidates: Vec<ReconcileCandidate>,
    failures: &mut Vec<String>,
) -> (Vec<LegacyStart>, Vec<ReconcileCandidate>) {
    let mut legacy = Vec::new();
    let mut serial = Vec::new();
    for candidate in candidates {
        if !candidate.state.enabled || candidate.state.active_version.is_empty() {
            serial.push(candidate);
            continue;
        }
        match load_installed(&candidate.id, &candidate.state.active_version) {
            Ok(extension) if extension.manifest.controller.is_none() => {
                legacy.push(LegacyStart {
                    id: candidate.id,
                    extension,
                    state: candidate.state,
                });
            }
            Ok(_) => serial.push(candidate),
            Err(err) => failures.push(format!("{}: {err}", candidate.id)),
        }
    }
    (legacy, serial)
}

fn start_legacy_parallel(starts: &[LegacyStart], failures: &mut Vec<String>) {
    const MAX_PARALLEL_STARTS: usize = 2;
    for batch in starts.chunks(MAX_PARALLEL_STARTS) {
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(batch.len());
            for start in batch {
                handles.push((
                    start.id.as_str(),
                    scope.spawn(move || start_legacy(&start.extension, &start.state)),
                ));
            }
            for (id, handle) in handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => failures.push(format!("{id}: {err}")),
                    Err(_) => failures.push(format!("{id}: start worker panicked")),
                }
            }
        });
    }
}

fn prioritize_enabled(candidates: &mut [ReconcileCandidate]) {
    candidates.sort_by_key(|candidate| !candidate.state.enabled);
}

fn reconcile_one(host: &Host, id: &str, state: &PluginState) -> Result<(), String> {
    if state.active_version.is_empty() {
        return Ok(());
    }
    let extension = load_installed(id, &state.active_version)?;
    if extension.manifest.controller.is_none() {
        if state.enabled {
            start_legacy(&extension, &state)
        } else {
            stop_legacy(id)
        }
    } else if state.enabled {
        apply_controller(
            host,
            &extension,
            &state,
            "reconcile",
            "core_ready",
            &[],
            "",
            None,
        )
        .map(|_| ())
    } else {
        cleanup_disabled_controller(id)
    }
}

// Controller actions can only supervise processes inside the per-extension
// helper.  Once that helper and any old transient units are stopped, a
// disabled extension has no runtime state left to reconcile.  Avoid spawning a
// fully sandboxed helper merely to return remove_all during every cold boot.
fn cleanup_disabled_controller(id: &str) -> Result<(), String> {
    stop_helper(id)?;
    stop_legacy_unit(id)?;
    stop_legacy_controller_units(id)?;
    remove_credential(id);
    let runtime = Path::new(paths::RUNTIME_ROOT).join(id);
    match fs::remove_dir_all(runtime) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("remove disabled extension runtime: {err}")),
    }
}

pub fn validate_device_variant(manifest: &Manifest) -> Result<(), String> {
    if !manifest.device_compatibility.is_empty() {
        let machine = current_machine()?;
        for compatibility in &manifest.device_compatibility {
            if compatibility.machine != machine {
                continue;
            }
            if compatibility.variants.is_empty() {
                return Ok(());
            }
            return validate_current_variant(&manifest.id, &compatibility.variants);
        }
        return Err(format!(
            "extension {} does not support machine {machine}",
            manifest.id
        ));
    }
    if manifest.device_variants.is_empty() {
        return Ok(());
    }
    validate_current_variant(&manifest.id, &manifest.device_variants)
}

fn current_machine() -> Result<String, String> {
    let data = fs::read(paths::MACHINE_PROFILE).map_err(|_| "read machine identity".to_string())?;
    let value: Value =
        serde_json::from_slice(&data).map_err(|_| "machine identity is unavailable".to_string())?;
    let machine = value
        .get("machine")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if paths::valid_id(machine) {
        Ok(machine.to_owned())
    } else {
        Err("machine identity is unavailable".into())
    }
}

fn validate_current_variant(id: &str, variants: &[String]) -> Result<(), String> {
    let data = fs::read(paths::MACHINE_HARDWARE).map_err(|_| "read device variant".to_string())?;
    let value: Value =
        serde_json::from_slice(&data).map_err(|_| "device variant is unavailable".to_string())?;
    let variant = value
        .get("variant")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if variants.iter().any(|candidate| candidate == variant) {
        Ok(())
    } else {
        Err(format!(
            "extension {id} does not support device variant {variant}"
        ))
    }
}

fn start_legacy(extension: &Installed, state: &PluginState) -> Result<(), String> {
    eprintln!("extension {}: preparing legacy service", extension.manifest.id);
    validate_device_variant(&extension.manifest)?;
    crate::host::validate_conflicts(&extension.manifest.id, &extension.manifest)?;
    load_kernel_modules(extension)?;
    if is_active(&paths::unit_name(&extension.manifest.id)) {
        return Ok(());
    }
    prepare_runtime_dirs(&extension.manifest.id)?;
    let settings = settings_for_state(extension, state)?;
    write_runtime_settings(&extension.manifest.id, &settings)?;
    if !service_enabled(&settings) {
        return Ok(());
    }
    let credential = prepare_credential(&extension.manifest)?;
    let mut arguments = vec![
        "--quiet".into(),
        "--collect".into(),
        format!("--unit={}", paths::unit_name(&extension.manifest.id)),
        format!("--slice={}", paths::EXTENSIONS_SLICE),
        "--property=Restart=on-failure".into(),
        "--property=RestartSec=1s".into(),
        "--property=PartOf=onekvm.service".into(),
    ];
    arguments.extend(sandbox_arguments(extension));
    arguments.extend([
        format!("--property=LoadCredential=onekvm-token:{credential}"),
        format!("--working-directory={}", extension.root.display()),
        format!("--setenv=ONEKVM_EXTENSION_ID={}", extension.manifest.id),
        format!(
            "--setenv=ONEKVM_EXTENSION_DATA={}",
            Path::new(paths::APP_DATA_ROOT)
                .join(&extension.manifest.id)
                .display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_CONFIG={}",
            Path::new(paths::RUNTIME_ROOT)
                .join(&extension.manifest.id)
                .display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_SOCKET={}",
            Path::new(paths::RUNTIME_ROOT)
                .join(&extension.manifest.id)
                .join("sockets")
                .display()
        ),
        "--".into(),
        extension
            .root
            .join(&extension.manifest.entrypoint)
            .to_string_lossy()
            .into_owned(),
    ]);
    eprintln!("extension {}: submitting transient service", extension.manifest.id);
    if let Err(err) = systemd_run(&arguments) {
        remove_credential(&extension.manifest.id);
        let _ = fs::remove_dir_all(Path::new(paths::RUNTIME_ROOT).join(&extension.manifest.id));
        return Err(err);
    }
    std::thread::sleep(Duration::from_millis(200));
    if !is_active(&paths::unit_name(&extension.manifest.id)) {
        remove_credential(&extension.manifest.id);
        let _ = fs::remove_dir_all(Path::new(paths::RUNTIME_ROOT).join(&extension.manifest.id));
        return Err(format!(
            "extension {} exited during startup",
            extension.manifest.id
        ));
    }
    eprintln!("extension {}: legacy service is active", extension.manifest.id);
    Ok(())
}

fn stop_legacy(id: &str) -> Result<(), String> {
    remove_credential(id);
    let _ = fs::remove_dir_all(Path::new(paths::RUNTIME_ROOT).join(id));
    if !is_active(&paths::unit_name(id)) {
        let _ = crate::sandbox::systemctl(&["reset-failed", &paths::unit_name(id)]);
        return Ok(());
    }
    crate::sandbox::systemctl(&["stop", &paths::unit_name(id)])?;
    let _ = crate::sandbox::systemctl(&["reset-failed", &paths::unit_name(id)]);
    Ok(())
}

fn service_enabled(settings: &Value) -> bool {
    match settings.get("service_enabled") {
        None => true,
        Some(Value::Bool(value)) => *value,
        _ => true,
    }
}

pub fn validate_conflicts(id: &str, manifest: &Manifest) -> Result<(), String> {
    if manifest.activation_conflicts.is_empty() {
        return Ok(());
    }
    let machine = current_machine()?;
    for conflict in &manifest.activation_conflicts {
        if !conflict.machines.is_empty() && !conflict.machines.iter().any(|item| item == &machine) {
            continue;
        }
        let state = read_state(&conflict.extension)?;
        if state.enabled {
            return Err(format!(
                "extension {id} conflicts with enabled extension {} on machine {machine}",
                conflict.extension
            ));
        }
    }
    Ok(())
}

fn ensure_helper(extension: &Installed) -> Result<bool, String> {
    let id = &extension.manifest.id;
    let reuse_key = format!("sandbox-v1:{}", extension.manifest.version);
    if helper_ready(id, &reuse_key) {
        return Ok(false);
    }
    stop_helper(id)?;
    stop_legacy_unit(id)?;
    stop_legacy_controller_units(id)?;
    let credential = prepare_credential(&extension.manifest)?;
    let address = paths::helper_address(id);
    let _ = fs::remove_file(&address);
    let controller = extension
        .manifest
        .controller
        .as_ref()
        .ok_or_else(|| format!("extension {id} has no controller"))?;
    let mut arguments = vec![
        "--quiet".into(),
        "--collect".into(),
        format!("--unit={}", paths::helper_unit(id)),
        format!("--slice={}", paths::EXTENSIONS_SLICE),
        "--property=Restart=no".into(),
        "--property=KillMode=control-group".into(),
        "--property=TimeoutStopSec=5s".into(),
        "--property=BindsTo=onekvm.service".into(),
        "--property=After=onekvm.service".into(),
    ];
    arguments.extend(helper_sandbox_arguments(extension));
    arguments.extend([
        format!("--property=LoadCredential=onekvm-token:{credential}"),
        format!("--working-directory={}", extension.root.display()),
        format!("--setenv=ONEKVM_EXTENSION_ID={id}"),
        format!(
            "--setenv=ONEKVM_EXTENSION_DATA={}",
            Path::new(paths::APP_DATA_ROOT).join(id).display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_CONFIG={}",
            Path::new(paths::RUNTIME_ROOT).join(id).display()
        ),
        format!(
            "--setenv=ONEKVM_EXTENSION_SOCKET={}",
            Path::new(paths::RUNTIME_ROOT)
                .join(id)
                .join("sockets")
                .display()
        ),
        "--".into(),
        paths::HOST_BINARY.into(),
        "--plugin-helper".into(),
        id.clone(),
        controller.timeout_seconds.to_string(),
        address.to_string_lossy().into_owned(),
        reuse_key.clone(),
        extension
            .root
            .join(&controller.entrypoint)
            .to_string_lossy()
            .into_owned(),
    ]);
    arguments.extend(controller.arguments.iter().cloned());
    systemd_run(&arguments).map_err(|err| format!("start extension helper: {err}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if helper_ready(id, &reuse_key) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = stop_helper(id);
    Err(format!("extension helper {id} did not become ready"))
}

fn helper_ready(id: &str, reuse_key: &str) -> bool {
    let Ok(address) = fs::read_to_string(paths::helper_address(id)) else {
        return false;
    };
    if address.lines().nth(1) != Some(reuse_key) {
        return false;
    }
    call_helper(id, "Ping", &[])
        .ok()
        .and_then(|value| value.get(0).and_then(Value::as_str).map(|text| text == id))
        .unwrap_or(false)
}

fn stop_helper(id: &str) -> Result<(), String> {
    let unit = paths::helper_unit(id);
    let address = paths::helper_address(id);
    if !address.exists() && !Path::new(paths::TRANSIENT_ROOT).join(&unit).exists() {
        return Ok(());
    }
    let _ = crate::sandbox::systemctl(&["stop", &unit]);
    let _ = fs::remove_file(address);
    Ok(())
}

fn stop_legacy_unit(id: &str) -> Result<(), String> {
    let unit = paths::unit_name(id);
    if !Path::new(paths::TRANSIENT_ROOT).join(&unit).exists() {
        return Ok(());
    }
    if !is_active(&unit) {
        let _ = crate::sandbox::systemctl(&["reset-failed", &unit]);
        return Ok(());
    }
    crate::sandbox::systemctl(&["stop", &unit])?;
    let _ = crate::sandbox::systemctl(&["reset-failed", &unit]);
    Ok(())
}

fn stop_legacy_controller_units(id: &str) -> Result<(), String> {
    let pattern = format!("onekvm-extension-{id}-*.service");
    let helper = paths::helper_unit(id);
    let mut has_legacy = false;
    if let Ok(entries) = fs::read_dir(paths::TRANSIENT_ROOT) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&format!("onekvm-extension-{id}-"))
                && name.ends_with(".service")
                && name != helper
            {
                has_legacy = true;
                break;
            }
        }
    }
    if !has_legacy {
        return remove_legacy_slice(id);
    }
    let _ = crate::sandbox::systemctl(&["stop", &pattern]);
    remove_legacy_slice(id)
}

fn remove_legacy_slice(id: &str) -> Result<(), String> {
    let name = format!(
        "system-onekvm-extensions-{}.slice",
        id.replace('-', r"\x2d")
    );
    let path = Path::new(paths::RUNTIME_UNIT_ROOT).join(&name);
    if path.exists() {
        let _ = crate::sandbox::systemctl(&["stop", &name]);
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn run_controller(
    extension: &Installed,
    payload: &Value,
    event: &str,
) -> Result<ControllerPlan, String> {
    let encoded = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let output = call_helper_bytes(&extension.manifest.id, "RunController", &encoded)?;
    if output.len() > 1 << 20 {
        return Err("controller output exceeds 1 MiB".into());
    }
    let plan: ControllerPlan =
        serde_json::from_slice(&output).map_err(|e| format!("decode controller plan: {e}"))?;
    if plan.actions.len() > 32 {
        return Err("controller plan contains too many actions".into());
    }
    let _ = event;
    Ok(plan)
}

fn execute_plan(extension: &Installed, plan: &ControllerPlan) -> Result<(), String> {
    for action in &plan.actions {
        execute_action(extension, action)
            .map_err(|err| format!("controller action {}/{}: {err}", action.kind, action.name))?;
    }
    Ok(())
}

fn execute_action(extension: &Installed, action: &ControllerAction) -> Result<(), String> {
    if action.kind != "remove_all" && !paths::valid_method_name(&action.name) {
        return Err(format!("invalid action instance name {:?}", action.name));
    }
    match action.kind.as_str() {
        "ensure" => ensure_instance(extension, action),
        "remove" => {
            call_helper(&extension.manifest.id, "Remove", &[json!(action.name)]).map(|_| ())
        }
        "remove_all" => call_helper(&extension.manifest.id, "RemoveAll", &[]).map(|_| ()),
        "exec" => exec_action(extension, action),
        other => Err(format!("unsupported action type {other:?}")),
    }
}

fn ensure_instance(extension: &Installed, action: &ControllerAction) -> Result<(), String> {
    validate_command(extension, &action.command)?;
    let restart = if action.restart.is_empty() {
        "on-failure"
    } else {
        action.restart.as_str()
    };
    if !matches!(restart, "no" | "on-failure" | "always") {
        return Err(format!("unsupported restart policy {restart:?}"));
    }
    let mut command = action.command.clone();
    command[0] = extension
        .root
        .join(&action.command[0])
        .to_string_lossy()
        .into_owned();
    let mut environment = Vec::new();
    for (key, value) in &action.environment {
        environment.push(format!("{key}={value}"));
    }
    call_helper(
        &extension.manifest.id,
        "Ensure",
        &[
            json!(action.name),
            json!(command),
            json!(environment),
            json!(restart),
        ],
    )
    .map(|_| ())
}

fn exec_action(extension: &Installed, action: &ControllerAction) -> Result<(), String> {
    validate_command(extension, &action.command)?;
    let timeout = if action.timeout_seconds == 0 {
        10
    } else {
        action.timeout_seconds
    };
    if !(1..=30).contains(&timeout) {
        return Err("action timeout must be between 1 and 30 seconds".into());
    }
    let mut command = action.command.clone();
    command[0] = extension
        .root
        .join(&action.command[0])
        .to_string_lossy()
        .into_owned();
    let environment: Vec<String> = action
        .environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    call_helper(
        &extension.manifest.id,
        "Exec",
        &[json!(command), json!(environment), json!(timeout)],
    )
    .map(|_| ())
}

fn validate_command(extension: &Installed, command: &[String]) -> Result<(), String> {
    if command.is_empty() || command[0].starts_with('/') || command[0].contains("..") {
        return Err("action command must start with a safe extension-relative executable".into());
    }
    let path = extension.root.join(&command[0]);
    let meta = fs::metadata(&path).map_err(|_| {
        format!(
            "action executable {} is missing or escapes extension root",
            command[0]
        )
    })?;
    if !meta.is_file() {
        return Err(format!(
            "action executable {} is not executable",
            command[0]
        ));
    }
    Ok(())
}

fn call_helper(id: &str, method: &str, args: &[Value]) -> Result<Vec<Value>, String> {
    crate::helper_client::call(id, method, args)
}

fn call_helper_bytes(id: &str, method: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    if method != "RunController" {
        return Err("unsupported helper byte method".into());
    }
    crate::helper_client::run_controller(id, payload)
}

pub fn core_lost(host: &Host) {
    if let Ok(mut generation) = host.generation.lock() {
        generation.clear();
    }
    let _ = fs::remove_file(paths::GENERATION_PATH);
    let _ = crate::sandbox::systemctl(&["stop", paths::EXTENSIONS_SLICE]);
}

pub fn repair_permissions() -> Result<(), String> {
    repair_settings_output_permissions()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, enabled: bool) -> ReconcileCandidate {
        ReconcileCandidate {
            id: id.into(),
            state: PluginState {
                enabled,
                ..PluginState::default()
            },
        }
    }

    #[test]
    fn reconcile_prioritizes_enabled_extensions() {
        let mut candidates = vec![
            candidate("disabled-a", false),
            candidate("enabled-a", true),
            candidate("disabled-b", false),
            candidate("enabled-b", true),
        ];
        prioritize_enabled(&mut candidates);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            vec!["enabled-a", "enabled-b", "disabled-a", "disabled-b"]
        );
    }
}
