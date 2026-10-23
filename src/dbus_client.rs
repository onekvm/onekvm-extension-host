use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

extern "C" {
    fn onekvm_cli_host_simple(
        method: *const c_char,
        id: *const c_char,
        revision: u64,
        err: *mut *mut c_char,
    ) -> c_int;
    fn onekvm_cli_host_invoke(
        id: *const c_char,
        method: *const c_char,
        request: *const c_char,
        response: *mut *mut c_char,
        err: *mut *mut c_char,
    ) -> c_int;
    fn onekvm_cli_host_apply_config(
        id: *const c_char,
        revision: u64,
        settings: *const c_char,
        keys: *mut *mut c_char,
        nkeys: usize,
        err: *mut *mut c_char,
    ) -> c_int;
    fn onekvm_cli_dbus_call_s(
        dest: *const c_char,
        path: *const c_char,
        iface: *const c_char,
        method: *const c_char,
        arg: *const c_char,
        has_arg: c_int,
        response: *mut *mut c_char,
        want_response: c_int,
        err: *mut *mut c_char,
    ) -> c_int;
}

fn c_string(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| "NUL in string".into())
}

unsafe fn take_string(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    libc::free(ptr.cast());
    text
}

fn fail(status: c_int, err: *mut c_char, what: &str) -> String {
    let message = unsafe { take_string(err) };
    if message.is_empty() {
        format!("{what} failed ({status})")
    } else {
        message
    }
}

pub fn enable(id: &str, revision: u64) -> Result<(), String> {
    host_simple("Enable", id, revision)
}

pub fn disable(id: &str, revision: u64) -> Result<(), String> {
    host_simple("Disable", id, revision)
}

pub fn reconcile(id: &str, revision: u64) -> Result<(), String> {
    host_simple("Reconcile", id, revision)
}

pub fn load_network_module(module: &str) -> Result<(), String> {
    host_simple("LoadNetworkModule", module, 0)
}

pub fn health(id: &str, revision: u64) -> Result<(), String> {
    host_simple("CheckHealth", id, revision)
}

pub fn transaction(operation: &str, id: &str) -> Result<(), String> {
    host_simple(operation, id, 0)
}

fn host_simple(method: &str, id: &str, revision: u64) -> Result<(), String> {
    let method = c_string(method)?;
    let id = c_string(id)?;
    let mut err = std::ptr::null_mut();
    let status =
        unsafe { onekvm_cli_host_simple(method.as_ptr(), id.as_ptr(), revision, &mut err) };
    if status != 0 {
        return Err(fail(status, err, "extension host"));
    }
    Ok(())
}

pub fn invoke(id: &str, method: &str, request: &str) -> Result<String, String> {
    let id = c_string(id)?;
    let method = c_string(method)?;
    let request = c_string(request)?;
    let mut response = std::ptr::null_mut();
    let mut err = std::ptr::null_mut();
    let status = unsafe {
        onekvm_cli_host_invoke(
            id.as_ptr(),
            method.as_ptr(),
            request.as_ptr(),
            &mut response,
            &mut err,
        )
    };
    if status != 0 {
        unsafe { take_string(response) };
        return Err(fail(status, err, "invoke extension method"));
    }
    Ok(unsafe { take_string(response) })
}

pub fn apply_config(
    id: &str,
    revision: u64,
    settings: &str,
    keys: &[String],
) -> Result<(), String> {
    let id = c_string(id)?;
    let settings = c_string(settings)?;
    let owned: Result<Vec<CString>, String> = keys.iter().map(|key| c_string(key)).collect();
    let owned = owned?;
    let mut pointers: Vec<*mut c_char> = owned
        .iter()
        .map(|value| value.as_ptr() as *mut c_char)
        .collect();
    let mut err = std::ptr::null_mut();
    let status = unsafe {
        onekvm_cli_host_apply_config(
            id.as_ptr(),
            revision,
            settings.as_ptr(),
            pointers.as_mut_ptr(),
            pointers.len(),
            &mut err,
        )
    };
    if status != 0 {
        return Err(fail(status, err, "extension host ApplyConfig"));
    }
    Ok(())
}

pub fn call_string(
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    arg: Option<&str>,
    want_response: bool,
) -> Result<String, String> {
    let dest = c_string(dest)?;
    let path = c_string(path)?;
    let iface = c_string(iface)?;
    let method = c_string(method)?;
    let payload = c_string(arg.unwrap_or_default())?;
    let mut response = std::ptr::null_mut();
    let mut err = std::ptr::null_mut();
    let status = unsafe {
        onekvm_cli_dbus_call_s(
            dest.as_ptr(),
            path.as_ptr(),
            iface.as_ptr(),
            method.as_ptr(),
            payload.as_ptr(),
            i32::from(arg.is_some()),
            if want_response {
                &mut response
            } else {
                std::ptr::null_mut()
            },
            i32::from(want_response),
            &mut err,
        )
    };
    if status != 0 {
        unsafe { take_string(response) };
        // sd_bus_call returns >0 on success; the C helper used to pass that
        // through for fire-and-forget UpdateSettings. Treat it as success.
        if !want_response && status > 0 && err.is_null() {
            return Ok(String::new());
        }
        return Err(fail(status, err, "system plugin D-Bus"));
    }
    Ok(unsafe { take_string(response) })
}
