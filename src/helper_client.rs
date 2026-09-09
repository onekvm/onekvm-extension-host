use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

use serde_json::Value;

use crate::paths;

extern "C" {
    fn onekvm_helper_client_ping(unique: *const c_char, id: *mut *mut c_char) -> c_int;
    fn onekvm_helper_client_run_controller(
        unique: *const c_char,
        payload: *const c_void,
        size: usize,
        output: *mut *mut c_void,
        out_size: *mut usize,
    ) -> c_int;
    fn onekvm_helper_client_ensure(
        unique: *const c_char,
        name: *const c_char,
        command: *mut *mut c_char,
        ncommand: usize,
        environment: *mut *mut c_char,
        nenv: usize,
        restart: *const c_char,
    ) -> c_int;
    fn onekvm_helper_client_simple(
        unique: *const c_char,
        method: *const c_char,
        name: *const c_char,
    ) -> c_int;
    fn onekvm_helper_client_exec(
        unique: *const c_char,
        command: *mut *mut c_char,
        ncommand: usize,
        environment: *mut *mut c_char,
        nenv: usize,
        timeout: u32,
    ) -> c_int;
    fn onekvm_helper_client_exec_capture(
        unique: *const c_char,
        command: *mut *mut c_char,
        ncommand: usize,
        environment: *mut *mut c_char,
        nenv: usize,
        payload: *const c_void,
        size: usize,
        timeout: u32,
        output: *mut *mut c_void,
        out_size: *mut usize,
    ) -> c_int;
}

fn unique_name(id: &str) -> Result<CString, String> {
    let data = std::fs::read_to_string(paths::helper_address(id))
        .map_err(|e| format!("read extension helper address: {e}"))?;
    let name = data.lines().next().unwrap_or_default();
    if name.len() > 128 || !name.starts_with(':') {
        return Err("extension helper address is invalid".into());
    }
    CString::new(name).map_err(|_| "extension helper address is invalid".into())
}

fn c_string(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| "NUL in string".into())
}

struct CStrings {
    owned: Vec<CString>,
    pointers: Vec<*mut c_char>,
}

impl CStrings {
    fn from_values(values: &[String]) -> Result<Self, String> {
        let mut owned = Vec::new();
        let mut pointers = Vec::new();
        for value in values {
            let item = c_string(value)?;
            pointers.push(item.as_ptr() as *mut c_char);
            owned.push(item);
        }
        Ok(Self { owned, pointers })
    }
}

fn fail(status: c_int, what: &str) -> String {
    format!("{what} failed ({status})")
}

pub fn call(id: &str, method: &str, args: &[Value]) -> Result<Vec<Value>, String> {
    let unique = unique_name(id)?;
    match method {
        "Ping" => {
            let mut id_ptr: *mut c_char = std::ptr::null_mut();
            let status = unsafe { onekvm_helper_client_ping(unique.as_ptr(), &mut id_ptr) };
            if status != 0 {
                return Err(fail(status, "Ping"));
            }
            let text = unsafe { take_string(id_ptr) };
            Ok(vec![Value::String(text)])
        }
        "Remove" | "Restart" => {
            let name = args
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| "helper name is required".to_string())?;
            let name = c_string(name)?;
            let method = c_string(method)?;
            let status = unsafe {
                onekvm_helper_client_simple(unique.as_ptr(), method.as_ptr(), name.as_ptr())
            };
            if status != 0 {
                return Err(fail(status, method.to_str().unwrap_or("helper")));
            }
            Ok(Vec::new())
        }
        "RemoveAll" => {
            let method = c_string("RemoveAll")?;
            let status = unsafe {
                onekvm_helper_client_simple(unique.as_ptr(), method.as_ptr(), std::ptr::null())
            };
            if status != 0 {
                return Err(fail(status, "RemoveAll"));
            }
            Ok(Vec::new())
        }
        "Ensure" => {
            let name = args
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| "ensure name is required".to_string())?;
            let command = string_list(args.get(1))?;
            let environment = string_list(args.get(2))?;
            let restart = args.get(3).and_then(Value::as_str).unwrap_or("on-failure");
            let name = c_string(name)?;
            let restart = c_string(restart)?;
            let mut command = CStrings::from_values(&command)?;
            let mut environment = CStrings::from_values(&environment)?;
            let status = unsafe {
                onekvm_helper_client_ensure(
                    unique.as_ptr(),
                    name.as_ptr(),
                    command.pointers.as_mut_ptr(),
                    command.owned.len(),
                    environment.pointers.as_mut_ptr(),
                    environment.owned.len(),
                    restart.as_ptr(),
                )
            };
            if status != 0 {
                return Err(fail(status, "Ensure"));
            }
            Ok(Vec::new())
        }
        "Exec" => {
            let command = string_list(args.first())?;
            let environment = string_list(args.get(1))?;
            let timeout = args.get(2).and_then(Value::as_u64).unwrap_or(10) as u32;
            let mut command = CStrings::from_values(&command)?;
            let mut environment = CStrings::from_values(&environment)?;
            let status = unsafe {
                onekvm_helper_client_exec(
                    unique.as_ptr(),
                    command.pointers.as_mut_ptr(),
                    command.owned.len(),
                    environment.pointers.as_mut_ptr(),
                    environment.owned.len(),
                    timeout,
                )
            };
            if status != 0 {
                return Err(fail(status, "Exec"));
            }
            Ok(Vec::new())
        }
        other => Err(format!("unsupported helper method {other}")),
    }
}

pub fn run_controller(id: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    let unique = unique_name(id)?;
    let mut output = std::ptr::null_mut();
    let mut size = 0usize;
    let status = unsafe {
        onekvm_helper_client_run_controller(
            unique.as_ptr(),
            payload.as_ptr().cast(),
            payload.len(),
            &mut output,
            &mut size,
        )
    };
    if status != 0 {
        return Err(fail(status, "RunController"));
    }
    let slice = unsafe { std::slice::from_raw_parts(output as *const u8, size) };
    let data = slice.to_vec();
    unsafe { libc::free(output) };
    Ok(data)
}

pub fn exec_capture(
    id: &str,
    command: &[String],
    environment: &[String],
    payload: &[u8],
    timeout: u32,
) -> Result<Vec<u8>, String> {
    let unique = unique_name(id)?;
    let mut command = CStrings::from_values(command)?;
    let mut environment = CStrings::from_values(environment)?;
    let mut output = std::ptr::null_mut();
    let mut size = 0usize;
    let status = unsafe {
        onekvm_helper_client_exec_capture(
            unique.as_ptr(),
            command.pointers.as_mut_ptr(),
            command.owned.len(),
            environment.pointers.as_mut_ptr(),
            environment.owned.len(),
            payload.as_ptr().cast(),
            payload.len(),
            timeout,
            &mut output,
            &mut size,
        )
    };
    if status != 0 {
        return Err(fail(status, "ExecCapture"));
    }
    let slice = unsafe { std::slice::from_raw_parts(output as *const u8, size) };
    let data = slice.to_vec();
    unsafe { libc::free(output) };
    Ok(data)
}

fn string_list(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(Value::Array(items)) = value else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "helper argument must be a string".to_string())
        })
        .collect()
}

unsafe fn take_string(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    libc::free(ptr.cast());
    text
}
