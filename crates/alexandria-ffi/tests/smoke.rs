use std::ffi::CStr;

#[test]
fn given_ffi_library_when_version_called_then_returns_version_string() {
    let raw = alexandria_ffi::alexandria_version();
    assert!(!raw.is_null());

    let cstr = unsafe { CStr::from_ptr(raw) };
    assert_eq!(cstr.to_str().unwrap(), "0.1.0");
}

#[test]
fn given_ffi_library_when_health_status_code_called_then_returns_200() {
    assert_eq!(alexandria_ffi::alexandria_health_status_code(), 200);
}
