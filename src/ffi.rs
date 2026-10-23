use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::slice;

use serde_json::Value;

use crate::host::{self, Host};

extern "C" {
    pub fn onekvm_host_serve(host: *mut c_void) -> c_int;
    pub fn onekvm_helper_serve(helper: *mut c_void) -> c_int;
}

fn err_ptr(slot: *mut *mut c_char, message: String) {
    let text = CString::new(message).unwrap_or_else(|_| CString::new("invalid error").unwrap());
    unsafe { *slot = text.into_raw() };
}

fn cstr<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Ok("");
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| "argument is not UTF-8".to_string())
}

fn keys_from(ptr: *mut *mut c_char, count: usize) -> Result<Vec<String>, String> {
    if ptr.is_null() || count == 0 {
        return Ok(Vec::new());
    }
    let slice = unsafe { slice::from_raw_parts(ptr, count) };
    slice
        .iter()
        .map(|item| cstr(*item).map(str::to_owned))
        .collect()
}

fn host_ref<'a>(ptr: *mut c_void) -> Result<&'a Host, String> {
    unsafe { ptr.cast::<Host>().as_ref() }.ok_or_else(|| "host is missing".to_string())
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_host_apply_config(
    host: *mut c_void,
    id: *const c_char,
    revision: u64,
    settings: *const c_char,
    keys: *mut *mut c_char,
    nkeys: usize,
    err: *mut *mut c_char,
) -> c_int {
    let result = (|| {
        let host = host_ref(host)?;
        let _settings = cstr(settings)?;
        let changed = keys_from(keys, nkeys)?;
        host::apply(
            host,
            cstr(id)?,
            revision,
            "config_changed",
            "configure",
            &changed,
            "",
            None,
        )
        .map(|_| ())
    })();
    match result {
        Ok(()) => 0,
        Err(message) => {
            err_ptr(err, message);
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_host_simple(
    host: *mut c_void,
    method: *const c_char,
    id: *const c_char,
    revision: u64,
    err: *mut *mut c_char,
) -> c_int {
    let result = (|| {
        let host = host_ref(host)?;
        let method = cstr(method)?;
        if method == "LoadNetworkModule" {
            if revision != 0 {
                return Err("network module revision must be zero".into());
            }
            return crate::network_modules::load(cstr(id)?);
        }
        if matches!(method, "CaptureTransaction" | "RestoreTransaction") {
            if revision != 0 {
                return Err("transaction revision must be zero".into());
            }
            return crate::transaction::payload_operation(
                cstr(id)?,
                method == "CaptureTransaction",
            );
        }
        if method == "CheckHealth" {
            return host::check_health(cstr(id)?, revision);
        }
        let (event, reason) = match method {
            "Enable" => ("enable", "enable"),
            "Disable" => ("disable", "disable"),
            "Reconcile" => ("reconcile", "reconcile"),
            other => return Err(format!("unsupported host method {other}")),
        };
        host::apply(host, cstr(id)?, revision, event, reason, &[], "", None).map(|_| ())
    })();
    match result {
        Ok(()) => 0,
        Err(message) => {
            err_ptr(err, message);
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_host_reconcile_all(
    host: *mut c_void,
    generation: *const c_char,
    err: *mut *mut c_char,
) -> c_int {
    let result = host_ref(host).and_then(|host| host::reconcile_all(host, cstr(generation)?));
    match result {
        Ok(()) => 0,
        Err(message) => {
            err_ptr(err, message);
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_host_prewarm(host: *mut c_void, err: *mut *mut c_char) -> c_int {
    let result = host_ref(host).and_then(|_| host::prewarm());
    match result {
        Ok(()) => 0,
        Err(message) => {
            err_ptr(err, message);
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_host_invoke(
    host: *mut c_void,
    id: *const c_char,
    method: *const c_char,
    request: *const c_char,
    response: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    let result = (|| {
        let host = host_ref(host)?;
        let raw = cstr(request)?;
        let request = if raw.is_empty() {
            Some(Value::Null)
        } else {
            Some(
                serde_json::from_str(raw)
                    .map_err(|_| "public method request is not valid JSON".to_string())?,
            )
        };
        let value = host::apply(
            host,
            cstr(id)?,
            0,
            "invoke",
            "invoke",
            &[],
            cstr(method)?,
            request,
        )?;
        Ok(if value.is_null() {
            "null".to_owned()
        } else {
            serde_json::to_string(&value).unwrap_or_else(|_| "null".into())
        })
    })();
    match result {
        Ok(text) => {
            unsafe { *response = CString::new(text).unwrap_or_default().into_raw() };
            0
        }
        Err(message) => {
            err_ptr(err, message);
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn onekvm_host_core_lost(host: *mut c_void) {
    if let Ok(host) = host_ref(host) {
        host::core_lost(host);
    }
}
